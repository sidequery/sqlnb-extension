#pragma once

// Result cache (docs/results.md). sqlnb_result_tee makes its statement's result
// collector pass the first rows on while the Rust library stores every row as
// Arrow; the other functions list, read back and drop stored results through
// DuckDB's Arrow scan.
#if __has_include("duckdb/main/query_result_stream.hpp")
#include "duckdb/common/arrow/arrow_appender.hpp"
#include "duckdb/common/arrow/arrow_converter.hpp"
#include "duckdb/common/arrow/arrow_wrapper.hpp"
#include "duckdb/common/type_visitor.hpp"
#include "duckdb/common/vector_operations/unary_executor.hpp"
#include "duckdb/execution/operator/helper/physical_result_collector.hpp"
#include "duckdb/execution/operator/helper/physical_result_sink.hpp"
#include "duckdb/function/scalar_function.hpp"
#include "duckdb/function/table/arrow.hpp"
#include "duckdb/function/table/arrow/arrow_duck_schema.hpp"
#include "duckdb/function/table_function.hpp"
#include "duckdb/main/client_config.hpp"
#include "duckdb/main/client_context.hpp"
#include "duckdb/main/client_context_state.hpp"
#include "duckdb/main/config.hpp"
#include "duckdb/main/extension/extension_loader.hpp"
#include "duckdb/main/prepared_statement_data.hpp"
#include "duckdb/parallel/base_pipeline_event.hpp"

#include <deque>
#include <map>

namespace duckdb {
namespace sqlnb_results {

static constexpr int64_t DEFAULT_HEAD_ROWS = 10240;
static constexpr int64_t DEFAULT_HEAD_BYTES = 4LL * 1024 * 1024;
// A stored record batch holds this many rows, or fewer once its buffers reach
// BATCH_BYTES, which bounds the memory one batch takes for very wide rows.
static constexpr idx_t BATCH_ROWS = 8192;
static constexpr idx_t BATCH_BYTES = 64 * 1024 * 1024;
static constexpr const char *AGENT_CONNECTION = "sqlnb_agent_connection";

// Marks the MCP agent's connection, which must not reach the result cache.
class AgentConnection : public ClientContextState {};

static void MarkAgentConnection(ClientContext &context) {
  context.registered_state->GetOrCreate<AgentConnection>(AGENT_CONNECTION);
}

static void RejectAgentConnection(ClientContext &context, const char *function) {
  if (context.registered_state->Get<AgentConnection>(AGENT_CONNECTION)) {
    throw InvalidInputException("POLICY_BLOCKED: the agent connection cannot call %s", function);
  }
}

[[noreturn]] static void ThrowCacheError(char *error) {
  string message = error ? error : "the result cache failed";
  sidequery_http_string_free(error);
  throw InvalidInputException("%s", message);
}

// Stored results use DuckDB's lossless Arrow types, whatever the caller's Arrow
// settings, so that UUID, HUGEINT, JSON and the like read back as themselves.
static ClientProperties StorageProperties(ClientContext &context) {
  return ClientProperties("UTC", ArrowOffsetSize::REGULAR, false, false, true, ArrowFormatVersion::V1_0,
                          &context);
}

// DuckDB's Arrow scan reads ENUMs back as VARCHAR, and aliased types (such as
// extension types) as their storage type unless an Arrow extension type keeps
// them, as it keeps JSON.
static bool ReadsBackAsAnotherType(ClientContext &context, const LogicalType &type) {
  auto &config = DBConfig::GetConfig(context);
  return TypeVisitor::Contains(type, [&](const LogicalType &child) {
    return child.id() == LogicalTypeId::ENUM || (child.HasAlias() && !config.HasArrowExtension(child));
  });
}

static void ExportSchema(ClientContext &context, const vector<LogicalType> &types, const vector<string> &names,
                         ArrowSchema &schema) {
  for (idx_t column = 0; column < types.size(); column++) {
    if (ReadsBackAsAnotherType(context, types[column])) {
      throw InvalidInputException("SQLNB_RESULT_UNSUPPORTED: column '%s' has type %s, which the result cache "
                                  "cannot read back unchanged",
                                  names[column], types[column].ToString());
    }
  }
  auto properties = StorageProperties(context);
  try {
    ArrowConverter::ToArrowSchema(&schema, types, names, properties);
  } catch (std::exception &error) {
    throw InvalidInputException("SQLNB_RESULT_UNSUPPORTED: %s", ErrorData(error).RawMessage());
  }
}

static const Value &NamedParameter(TableFunctionBindInput &input, const char *name) {
  auto &value = input.named_parameters.at(name);
  if (value.IsNull()) {
    throw InvalidInputException("%s must not be NULL", name);
  }
  return value;
}

static idx_t CountParameter(TableFunctionBindInput &input, const char *name) {
  auto count = NamedParameter(input, name).GetValue<int64_t>();
  if (count < 0) {
    throw InvalidInputException("%s must not be negative", name);
  }
  return NumericCast<idx_t>(count);
}

struct TeeParameters {
  string id;
  string meta;
  idx_t head_rows;
  idx_t head_bytes;
  idx_t max_result_bytes;
  idx_t max_cache_bytes;
  // Seconds the result is kept after its last use; 0 keeps it until DuckDB exits.
  idx_t ttl_seconds;
  bool stop_at_limit;
  vector<LogicalType> types;
  vector<string> names;
  // The statement made its result readable.
  atomic<bool> published {false};
};

struct TeeBindData : public TableFunctionData {
  shared_ptr<TeeParameters> parameters;
};

typedef unique_ptr<PhysicalOperator> (*tee_collector_t)(ClientContext &, PreparedStatementData &);
static unique_ptr<PhysicalOperator> TeeCollector(ClientContext &context, PreparedStatementData &data);

// Carries a tee from the bind of its statement to the statement's result
// collector, and takes the collector hook down when the statement ends.
class TeeStatement : public ClientContextState {
public:
  shared_ptr<TeeParameters> parameters;
  // The collector hook in place when the statement was bound, such as Quack's.
  get_result_collector_t previous;
  bool hooked = false;
  // Set once the statement's collector stores its result.
  bool collecting = false;

  void Unhook(ClientContext &context) {
    if (!hooked) {
      return;
    }
    hooked = false;
    auto &hook = ClientConfig::GetConfig(context).get_result_collector;
    auto installed = hook.target<tee_collector_t>();
    if (installed && *installed == TeeCollector) {
      // Nobody replaced the hook since the bind: the one it replaced comes back.
      hook = std::move(previous);
    }
    previous = nullptr;
  }

  void QueryEnd(ClientContext &context, optional_ptr<ErrorData> error) override {
    if (error && error->HasError() && parameters && parameters->published) {
      // The statement failed after its last pipeline had finished: DuckDB
      // raises an interrupt that arrives while that pipeline finalizes only
      // afterwards. A failed statement leaves no result.
      sidequery_result_drop(parameters->id.c_str());
    }
    Unhook(context);
    parameters.reset();
    collecting = false;
  }
};

// A result collector that hands every call to the collector it wraps: the one
// another extension installed for the statement. The wrapped collector gets
// its states with each call. Its own `sink_state` member stays unset, because
// DuckDB sets that on the pipeline's sink, which is this operator; a collector
// that read its own member instead of the state it is passed could not be
// wrapped. Quack's and DuckDB's collectors do not.
class DelegatingCollector : public PhysicalResultCollector {
public:
  DelegatingCollector(PhysicalPlan &plan, PreparedStatementData &data, unique_ptr<PhysicalOperator> inner_p)
      : PhysicalResultCollector(plan, data), inner(std::move(inner_p)) {
  }

  unique_ptr<GlobalSinkState> GetGlobalSinkState(ClientContext &context) const override {
    return inner->GetGlobalSinkState(context);
  }
  unique_ptr<LocalSinkState> GetLocalSinkState(ExecutionContext &context) const override {
    return inner->GetLocalSinkState(context);
  }
  SinkResultType Sink(ExecutionContext &context, DataChunk &chunk, OperatorSinkInput &input) const override {
    return inner->Sink(context, chunk, input);
  }
  SinkNextBatchType NextBatch(ExecutionContext &context, OperatorSinkNextBatchInput &input) const override {
    return inner->NextBatch(context, input);
  }
  SinkNextBatchType UpdateMinBatchIndex(ExecutionContext &context, OperatorSinkNextBatchInput &input) const override {
    return inner->UpdateMinBatchIndex(context, input);
  }
  SinkCombineResultType Combine(ExecutionContext &context, OperatorSinkCombineInput &input) const override {
    return inner->Combine(context, input);
  }
  void PrepareFinalize(ClientContext &context, GlobalSinkState &state) const override {
    inner->PrepareFinalize(context, state);
  }
  SinkFinalizeType Finalize(Pipeline &pipeline, Event &event, ClientContext &context,
                            OperatorSinkFinalizeInput &input) const override {
    return inner->Finalize(pipeline, event, context, input);
  }
  unique_ptr<QueryResult> GetResult(GlobalSinkState &state) const override {
    return Collector().GetResult(state);
  }
  ProgressData GetSinkProgress(ClientContext &context, GlobalSinkState &state,
                               const ProgressData source_progress) const override {
    return inner->GetSinkProgress(context, state, source_progress);
  }
  bool ParallelSink() const override {
    return inner->ParallelSink();
  }
  OperatorPartitionInfo RequiredPartitionInfo() const override {
    return inner->RequiredPartitionInfo();
  }
  bool SinkOrderDependent() const override {
    return inner->SinkOrderDependent();
  }
  PipelineExternalInputSupport GetExternalInputSupport() const override {
    return inner->GetExternalInputSupport();
  }
  bool IsStreaming() const override {
    return Collector().IsStreaming();
  }
  bool BuildsOwnResult() const override {
    return Collector().BuildsOwnResult();
  }

private:
  const PhysicalResultCollector &Collector() const {
    return inner->Cast<PhysicalResultCollector>();
  }

  unique_ptr<PhysicalOperator> inner;
};

// Rows held back for the head, with their size as Arrow.
struct TeeHeldChunk {
  unique_ptr<DataChunk> chunk;
  idx_t bytes;
};

// The head rows an ended executor batch left waiting.
struct TeeWaitingBatch {
  vector<TeeHeldChunk> chunks;
  idx_t rows = 0;
  idx_t bytes = 0;
};

class TeeLocalState;

class TeeGlobalState : public GlobalSinkState {
public:
  // An interrupted or failed statement never finishes its result.
  ~TeeGlobalState() override {
    if (writer) {
      sidequery_result_abort(writer);
    }
  }

  idx_t MaxThreads(idx_t source_max_threads) override {
    return inner->MaxThreads(source_max_threads);
  }

  unique_ptr<GlobalSinkState> inner;
  sidequery_result_writer *writer = nullptr;
  // The event that makes the result readable is in place.
  bool publish_scheduled = false;
  atomic<bool> storing {true};
  atomic<idx_t> rows {0};
  // Numbers the stored batches of a result that has no executor batches.
  atomic<idx_t> arrivals {0};
  // Set once the first rows of the result have been passed on.
  atomic<bool> head_done {false};
  atomic<idx_t> passed_rows {0};
  atomic<idx_t> passed_bytes {0};
  // The lowest executor batch still being read, as far as any thread knows.
  atomic<idx_t> min_batch {0};
  mutex head_lock;
  // Head rows of ended executor batches, waiting for the batches before them.
  // Only as many batches wait as can still be part of the head: the lowest of
  // them, up to the one with which they fill it. Without that, a slow lowest
  // batch would leave a head's worth of rows here for every batch that ends.
  map<idx_t, TeeWaitingBatch> waiting;
  idx_t waiting_rows = 0;
  idx_t waiting_bytes = 0;
  // The highest executor batch whose rows can still be part of the head: the
  // ended batches up to it fill the head. It only falls; set under head_lock.
  atomic<idx_t> head_cutoff {DConstants::INVALID_INDEX};
  // Threads that have not combined.
  idx_t threads = 0;
  // Without executor batches the head is the chunks claimed first, in the
  // order they were claimed. One thread at a time passes them on; the chunks
  // other threads claim meanwhile queue up for it.
  idx_t claims = 0;
  idx_t claimed_rows = 0;
  idx_t claimed_bytes = 0;
  TeeLocalState *passer = nullptr;
  std::deque<TeeHeldChunk> queue;
  // The passer's state in the wrapped collector when its sink is parallel:
  // that collector keeps the order of one state's rows, not of several. Only
  // the passer makes, uses and combines it. It sets and clears the pointer
  // under head_lock, where a thread that leaves the head tests it.
  unique_ptr<LocalSinkState> head_inner;
};

// Makes a statement's result readable, as the last thing its last pipeline
// does. The wrapped collector may finish its own work in events it inserts
// while it finalizes: Quack can send a result's last batches from one, which
// waits for the client and fails when the client is gone. A statement that
// fails there has failed, and leaves no result. This event is inserted before
// the wrapped collector finalizes, which puts it after every event inserted
// later (Event::InsertEvent), so it only runs once those have finished. A
// statement that still fails after this event is handled when it ends
// (TeeStatement::QueryEnd).
class TeePublishEvent : public BasePipelineEvent {
public:
  TeePublishEvent(Pipeline &pipeline, TeeGlobalState &global_p, shared_ptr<TeeParameters> parameters_p,
                  bool rows_exact_p)
      : BasePipelineEvent(pipeline), global(global_p), parameters(std::move(parameters_p)),
        rows_exact(rows_exact_p) {
  }

  void Schedule() override {
  }

  void FinishEvent() override {
    if (global.writer) {
      sidequery_result_finish(global.writer, global.rows, rows_exact);
      global.writer = nullptr;
      parameters->published = true;
    }
  }

private:
  TeeGlobalState &global;
  const shared_ptr<TeeParameters> parameters;
  const bool rows_exact;
};

class TeeLocalState : public LocalSinkState {
public:
  unique_ptr<LocalSinkState> inner;
  // Rows converted to Arrow that are not stored yet.
  unique_ptr<ArrowAppender> batch;
  // Stored batches of this thread: the minor part of their key.
  idx_t pieces = 0;
  // The executor batch being read; unset for a result without executor batches.
  optional_idx current_batch;
  // Rows and bytes of the current batch that may belong to the head.
  idx_t head_rows = 0;
  idx_t head_bytes = 0;
  // Head rows to pass on, in the result's order: those of the current batch
  // and, once it is the lowest, those of the ended batches before it.
  vector<TeeHeldChunk> held;
  idx_t held_passed = 0;
  // Every batch before the current one has ended and its head rows were taken.
  bool lowest = false;
  // The chunk in hand was stored; a blocked call returns here with it.
  bool chunk_taken = false;
  // The chunk in hand is passed on once the held chunks are.
  bool pass_chunk = false;
  idx_t chunk_bytes = 0;
  // Without executor batches: this thread passes the head on, and the
  // queued chunk it is passing.
  bool passer = false;
  TeeHeldChunk passing;
  bool reported_missing = false;
  // Combine: this thread is no longer counted, and the batch the last
  // waiting rows are passed on as, when that is not its own.
  bool left = false;
  optional_idx last_batch;
};

// Stores every row of a statement's result and passes only its first rows on
// to the collector BASE, which serves them to the client. It runs where that
// collector runs: on every thread of the statement's last pipeline.
//
// Threads store their rows as they produce them. A stored batch is keyed by
// its executor batch, and the result reads back in key order, so storing
// never waits for an earlier batch.
//
// Only the head needs the order while the statement runs, and the rows passed
// on are exactly the first rows of the result, whether or not the cache could
// store them: a cache that stops storing never shortens what the statement
// returns. The thread reading the lowest
// executor batch passes its rows on at once. Every other thread holds the
// first rows of its batch back; when the batch ends they wait in the global
// state for the thread whose batch becomes the lowest, or for the last thread
// to combine. A result without executor batches has no order to wait for:
// its head is the chunks that arrive first, stored first.
template <class BASE>
class TeeSink : public BASE {
public:
  template <class... ARGS>
  TeeSink(ClientContext &context, shared_ptr<TeeParameters> parameters_p, ARGS &&... args)
      : BASE(std::forward<ARGS>(args)...), parameters(std::move(parameters_p)),
        properties(StorageProperties(context)),
        extension_types(ArrowTypeExtensionData::GetExtensionTypes(context, parameters->types)) {
  }

  string GetName() const override {
    return "SQLNB_RESULT_TEE";
  }

  unique_ptr<GlobalSinkState> GetGlobalSinkState(ClientContext &context) const override {
    auto state = make_uniq<TeeGlobalState>();
    state->inner = BASE::GetGlobalSinkState(context);
    state->head_done = parameters->head_rows == 0 || parameters->head_bytes == 0;
    ArrowSchemaWrapper schema;
    ExportSchema(context, parameters->types, parameters->names, schema.arrow_schema);
    char *error = nullptr;
    state->writer = sidequery_result_begin(parameters->id.c_str(), parameters->meta.c_str(), &schema.arrow_schema,
                                           parameters->head_rows, parameters->max_result_bytes,
                                           parameters->max_cache_bytes, parameters->ttl_seconds, &error);
    if (!state->writer) {
      ThrowCacheError(error);
    }
    return std::move(state);
  }

  unique_ptr<LocalSinkState> GetLocalSinkState(ExecutionContext &context) const override {
    auto state = make_uniq<TeeLocalState>();
    state->inner = BASE::GetLocalSinkState(context);
    auto &global = this->sink_state->template Cast<TeeGlobalState>();
    lock_guard<mutex> guard(global.head_lock);
    global.threads++;
    return std::move(state);
  }

  SinkResultType Sink(ExecutionContext &context, DataChunk &chunk, OperatorSinkInput &input) const override {
    auto &global = input.global_state.template Cast<TeeGlobalState>();
    auto &local = input.local_state.template Cast<TeeLocalState>();
    if (!local.chunk_taken) {
      Take(global, local, chunk);
      local.chunk_taken = true;
    }
    if (!PassHead(context, global, local, chunk, input.interrupt_state)) {
      // The executor calls again with the same chunk.
      return SinkResultType::BLOCKED;
    }
    local.chunk_taken = false;
    if (StoppedAtLimit(global)) {
      // This thread reads no further, so the rest of its batch is not stored.
      ReportMissing(global, local);
      return SinkResultType::FINISHED;
    }
    return SinkResultType::NEED_MORE_INPUT;
  }

  // The executor has already moved partition_info to the next batch.
  SinkNextBatchType NextBatch(ExecutionContext &context, OperatorSinkNextBatchInput &input) const override {
    auto &global = input.global_state.template Cast<TeeGlobalState>();
    auto &local = input.local_state.template Cast<TeeLocalState>();
    if (local.current_batch.IsValid() && !EndBatch(context, global, local, input.interrupt_state)) {
      return SinkNextBatchType::BLOCKED;
    }
    SyncPartition(global, local, local.partition_info.batch_index);
    OperatorSinkNextBatchInput inner {*global.inner, *local.inner, input.interrupt_state};
    if (BASE::NextBatch(context, inner) == SinkNextBatchType::BLOCKED) {
      return SinkNextBatchType::BLOCKED;
    }
    StartBatch(local);
    return SinkNextBatchType::READY;
  }

  SinkNextBatchType UpdateMinBatchIndex(ExecutionContext &context,
                                        OperatorSinkNextBatchInput &input) const override {
    auto &global = input.global_state.template Cast<TeeGlobalState>();
    auto &local = input.local_state.template Cast<TeeLocalState>();
    SyncPartition(global, local, local.partition_info.batch_index);
    OperatorSinkNextBatchInput inner {*global.inner, *local.inner, input.interrupt_state};
    return BASE::UpdateMinBatchIndex(context, inner);
  }

  SinkCombineResultType Combine(ExecutionContext &context, OperatorSinkCombineInput &input) const override {
    auto &global = input.global_state.template Cast<TeeGlobalState>();
    auto &local = input.local_state.template Cast<TeeLocalState>();
    if (!LeaveHead(context, global, local, input.interrupt_state)) {
      return SinkCombineResultType::BLOCKED;
    }
    SyncPartition(global, local, InnerBatch(local));
    OperatorSinkCombineInput inner {*global.inner, *local.inner, input.interrupt_state};
    if (BASE::Combine(context, inner) == SinkCombineResultType::BLOCKED) {
      return SinkCombineResultType::BLOCKED;
    }
    return SinkCombineResultType::FINISHED;
  }

  void PrepareFinalize(ClientContext &context, GlobalSinkState &state) const override {
    BASE::PrepareFinalize(context, *state.template Cast<TeeGlobalState>().inner);
  }

  SinkFinalizeType Finalize(Pipeline &pipeline, Event &event, ClientContext &context,
                            OperatorSinkFinalizeInput &input) const override {
    auto &global = input.global_state.template Cast<TeeGlobalState>();
    if (!global.publish_scheduled) {
      // Every thread has combined, so the counts are final. A wrapped
      // collector that blocks is finalized again with the same event.
      global.publish_scheduled = true;
      event.InsertEvent(
          make_shared_ptr<TeePublishEvent>(pipeline, global, parameters, !StoppedAtLimit(global)));
    }
    OperatorSinkFinalizeInput inner {*global.inner, input.interrupt_state};
    return BASE::Finalize(pipeline, event, context, inner);
  }

  unique_ptr<QueryResult> GetResult(GlobalSinkState &state) const override {
    return BASE::GetResult(*state.template Cast<TeeGlobalState>().inner);
  }

  ProgressData GetSinkProgress(ClientContext &context, GlobalSinkState &state,
                               const ProgressData source_progress) const override {
    return BASE::GetSinkProgress(context, *state.template Cast<TeeGlobalState>().inner, source_progress);
  }

private:
  bool BatchOrdered() const {
    return this->RequiredPartitionInfo().RequiresBatchIndex();
  }

  // A statement the read policy verifies stops reading once nothing more is
  // stored. Not before its head is complete: threads that stopped earlier
  // would leave the head with rows from wherever each of them had got to.
  bool StoppedAtLimit(TeeGlobalState &global) const {
    return parameters->stop_at_limit && !global.storing && global.head_done;
  }

  void StartBatch(TeeLocalState &local) const {
    local.current_batch = local.partition_info.batch_index;
    local.head_rows = 0;
    local.head_bytes = 0;
    local.lowest = false;
  }

  // Stores a chunk and decides whether it may belong to the head.
  void Take(TeeGlobalState &global, TeeLocalState &local, DataChunk &chunk) const {
    auto batches = BatchOrdered();
    if (batches && !local.current_batch.IsValid()) {
      StartBatch(local);
    }
    global.rows += chunk.size();
    local.pass_chunk = false;
    if (!batches) {
      if (global.head_done) {
        Store(global, local, chunk);
      } else {
        ClaimChunk(global, local, chunk);
      }
      return;
    }
    auto bytes = Store(global, local, chunk);
    local.chunk_bytes = bytes;
    if (global.head_done) {
      return;
    }
    if (local.head_rows >= parameters->head_rows || local.head_bytes >= parameters->head_bytes) {
      // The batch has given the head all it can take.
      return;
    }
    local.head_rows += chunk.size();
    local.head_bytes += bytes;
    if (BecomeLowest(global, local)) {
      local.pass_chunk = true;
      return;
    }
    if (local.current_batch.GetIndex() > global.head_cutoff) {
      // Ended batches before this one fill the head. Nothing of this batch
      // was passed on, so all it holds is its own rows.
      DropHeld(local);
      return;
    }
    local.held.push_back(Copy(chunk, bytes));
  }

  // Without executor batches: makes the chunk the next of the head and
  // stores it there, unless the head got all its rows meanwhile.
  void ClaimChunk(TeeGlobalState &global, TeeLocalState &local, DataChunk &chunk) const {
    // The rows this thread has not stored yet come after the head.
    FlushStore(global, local);
    auto bytes = Convert(global, local, chunk);
    idx_t claim;
    {
      unique_lock<mutex> guard(global.head_lock);
      if (global.head_done) {
        guard.unlock();
        // The chunk is stored with the rows that follow it.
        if (!local.batch) {
          ReportMissing(global, local);
        }
        return;
      }
      claim = global.claims++;
      global.claimed_rows += chunk.size();
      global.claimed_bytes += bytes;
      if (global.claimed_rows >= parameters->head_rows || global.claimed_bytes >= parameters->head_bytes) {
        global.head_done = true;
      }
      if (!global.passer) {
        global.passer = &local;
        local.passer = true;
        local.pass_chunk = true;
      } else {
        global.queue.push_back(Copy(chunk, bytes));
      }
    }
    FlushStoreAs(global, local, 0, claim);
  }

  static TeeHeldChunk Copy(DataChunk &chunk, idx_t bytes) {
    TeeHeldChunk held;
    held.chunk = make_uniq<DataChunk>();
    held.chunk->Initialize(Allocator::DefaultAllocator(), chunk.GetTypes(), MaxValue<idx_t>(chunk.size(), 1));
    chunk.Copy(*held.chunk);
    held.bytes = bytes;
    return held;
  }

  // Without executor batches: passes one chunk of the head on. False when
  // the wrapped collector blocked without taking the chunk.
  bool PassClaimed(ExecutionContext &context, TeeGlobalState &global, TeeLocalState &local, DataChunk &chunk,
                   InterruptState &interrupt) const {
    auto state = local.inner.get();
    if (this->ParallelSink()) {
      if (!global.head_inner) {
        auto created = BASE::GetLocalSinkState(context);
        lock_guard<mutex> guard(global.head_lock);
        global.head_inner = std::move(created);
      }
      state = global.head_inner.get();
      state->partition_info = local.partition_info;
    } else {
      SyncPartition(global, local, InnerBatch(local));
    }
    OperatorSinkInput inner {*global.inner, *state, interrupt};
    return BASE::Sink(context, chunk, inner) != SinkResultType::BLOCKED;
  }

  // The passer passes on the chunks other threads claimed meanwhile, then
  // gives the role up. Once no more chunks can follow, it first combines the
  // state it passed them through. False when the wrapped collector blocked;
  // this thread stays the passer and the next call continues.
  bool FinishPassing(ExecutionContext &context, TeeGlobalState &global, TeeLocalState &local,
                     InterruptState &interrupt) const {
    while (true) {
      if (local.passing.chunk) {
        if (!PassClaimed(context, global, local, *local.passing.chunk, interrupt)) {
          return false;
        }
        local.passing.chunk.reset();
      }
      unique_lock<mutex> guard(global.head_lock);
      if (!global.queue.empty()) {
        local.passing = std::move(global.queue.front());
        global.queue.pop_front();
        continue;
      }
      if (global.head_inner && (global.head_done || global.threads == 0)) {
        guard.unlock();
        global.head_inner->partition_info = local.partition_info;
        OperatorSinkCombineInput inner {*global.inner, *global.head_inner, interrupt};
        if (BASE::Combine(context, inner) == SinkCombineResultType::BLOCKED) {
          return false;
        }
        // A thread that starts later passes its chunks through a new state.
        guard.lock();
        auto combined = std::move(global.head_inner);
        guard.unlock();
        continue;
      }
      global.passer = nullptr;
      local.passer = false;
      return true;
    }
  }

  // The lowest batch only rises. A thread learns it when its own batch
  // changes, so the threads share the highest value any of them has seen.
  idx_t PublishMinBatch(TeeGlobalState &global, TeeLocalState &local) const {
    auto seen = local.partition_info.min_batch_index.IsValid() ? local.partition_info.min_batch_index.GetIndex() : 0;
    auto known = global.min_batch.load();
    while (seen > known && !global.min_batch.compare_exchange_weak(known, seen)) {
    }
    return MaxValue(seen, known);
  }

  // Whether every executor batch before this thread's has ended. The first
  // time it has, the head rows those batches left waiting become this
  // thread's to pass on, before its own.
  bool BecomeLowest(TeeGlobalState &global, TeeLocalState &local) const {
    if (local.lowest) {
      return true;
    }
    if (local.current_batch.GetIndex() > PublishMinBatch(global, local)) {
      return false;
    }
    lock_guard<mutex> guard(global.head_lock);
    TakeWaitingBefore(global, local);
    return true;
  }

  // Requires head_lock. A batch leaves its rows waiting before the lowest
  // batch moves past it, so none arrive here later.
  void TakeWaitingBefore(TeeGlobalState &global, TeeLocalState &local) const {
    auto end = global.waiting.lower_bound(local.current_batch.GetIndex());
    vector<TeeHeldChunk> before;
    for (auto entry = global.waiting.begin(); entry != end; entry++) {
      global.waiting_rows -= entry->second.rows;
      global.waiting_bytes -= entry->second.bytes;
      for (auto &held : entry->second.chunks) {
        before.push_back(std::move(held));
      }
    }
    global.waiting.erase(global.waiting.begin(), end);
    if (!before.empty()) {
      // Nothing of this batch was passed on while a batch before it was read.
      D_ASSERT(local.held_passed == 0);
      for (auto &held : local.held) {
        before.push_back(std::move(held));
      }
      local.held = std::move(before);
    }
    local.lowest = true;
  }

  // The batch the wrapped collector is told this thread reads.
  static optional_idx InnerBatch(TeeLocalState &local) {
    return local.current_batch.IsValid() ? local.current_batch : local.partition_info.batch_index;
  }

  // Gives the wrapped collector the partition of the batch its next call is for.
  void SyncPartition(TeeGlobalState &global, TeeLocalState &local, optional_idx batch) const {
    local.inner->partition_info = local.partition_info;
    if (!BatchOrdered()) {
      return;
    }
    local.inner->partition_info.batch_index = batch;
    local.inner->partition_info.min_batch_index = PublishMinBatch(global, local);
  }

  // Passes one chunk to the wrapped collector. False when it blocked without
  // taking the chunk.
  bool PassChunk(ExecutionContext &context, TeeGlobalState &global, TeeLocalState &local, DataChunk &chunk,
                 idx_t bytes, InterruptState &interrupt) const {
    SyncPartition(global, local, InnerBatch(local));
    OperatorSinkInput inner {*global.inner, *local.inner, interrupt};
    if (BASE::Sink(context, chunk, inner) == SinkResultType::BLOCKED) {
      return false;
    }
    auto rows = global.passed_rows.fetch_add(chunk.size()) + chunk.size();
    auto passed = global.passed_bytes.fetch_add(bytes) + bytes;
    if (rows >= parameters->head_rows || passed >= parameters->head_bytes) {
      CloseHead(global, local);
    }
    return true;
  }

  void CloseHead(TeeGlobalState &global, TeeLocalState &local) const {
    global.head_done = true;
    lock_guard<mutex> guard(global.head_lock);
    ClearWaiting(global);
  }

  // Requires head_lock.
  static void ClearWaiting(TeeGlobalState &global) {
    global.waiting.clear();
    global.waiting_rows = 0;
    global.waiting_bytes = 0;
  }

  // Passes on the chunks this thread holds for the head. False when the
  // wrapped collector blocked; the next call continues where this one stopped.
  bool PassHeld(ExecutionContext &context, TeeGlobalState &global, TeeLocalState &local,
                InterruptState &interrupt) const {
    while (local.held_passed < local.held.size() && !global.head_done) {
      auto &held = local.held[local.held_passed];
      if (!PassChunk(context, global, local, *held.chunk, held.bytes, interrupt)) {
        return false;
      }
      local.held_passed++;
    }
    DropHeld(local);
    return true;
  }

  static void DropHeld(TeeLocalState &local) {
    local.held.clear();
    local.held_passed = 0;
  }

  // Passes on what the head needs from this thread now. `chunk` is the chunk
  // in hand. False when the wrapped collector blocked.
  bool PassHead(ExecutionContext &context, TeeGlobalState &global, TeeLocalState &local, DataChunk &chunk,
                InterruptState &interrupt) const {
    if (!BatchOrdered()) {
      if (!local.passer) {
        return true;
      }
      if (local.pass_chunk && !PassClaimed(context, global, local, chunk, interrupt)) {
        return false;
      }
      local.pass_chunk = false;
      return FinishPassing(context, global, local, interrupt);
    }
    if (!global.head_done && !BecomeLowest(global, local)) {
      return true;
    }
    if (!PassHeld(context, global, local, interrupt)) {
      return false;
    }
    if (local.pass_chunk && !global.head_done &&
        !PassChunk(context, global, local, chunk, local.chunk_bytes, interrupt)) {
      return false;
    }
    local.pass_chunk = false;
    return true;
  }

  // Stores what is left of this thread's batch. Its head rows are passed on
  // if every batch before it has ended, and wait for those batches if not.
  // False when the wrapped collector blocked.
  bool EndBatch(ExecutionContext &context, TeeGlobalState &global, TeeLocalState &local,
                InterruptState &interrupt) const {
    FlushStore(global, local);
    if (!global.head_done && !BecomeLowest(global, local)) {
      LeaveWaiting(global, local);
      return true;
    }
    return PassHeld(context, global, local, interrupt);
  }

  void LeaveWaiting(TeeGlobalState &global, TeeLocalState &local) const {
    if (local.held.empty()) {
      return;
    }
    lock_guard<mutex> guard(global.head_lock);
    WaitLocked(global, local);
  }

  // Requires head_lock. Leaves this thread's head rows waiting for the
  // batches before its own, unless they can no longer be part of the head.
  void WaitLocked(TeeGlobalState &global, TeeLocalState &local) const {
    auto batch = local.current_batch.GetIndex();
    if (!local.held.empty() && !global.head_done && batch <= global.head_cutoff) {
      auto &waiting = global.waiting[batch];
      for (auto &held : local.held) {
        waiting.rows += held.chunk->size();
        waiting.bytes += held.bytes;
        global.waiting_rows += held.chunk->size();
        global.waiting_bytes += held.bytes;
        waiting.chunks.push_back(std::move(held));
      }
      TrimWaiting(global);
    }
    DropHeld(local);
  }

  // Requires head_lock. The head takes the rows that wait in batch order, and
  // the batches still being read only add rows before them. So once the
  // batches that wait fill the head without the highest of them, that one can
  // never be part of it: it is dropped, and the batches after the highest one
  // left stop holding rows back.
  void TrimWaiting(TeeGlobalState &global) const {
    auto fills = [&](idx_t rows, idx_t bytes) {
      return rows >= parameters->head_rows || bytes >= parameters->head_bytes;
    };
    while (global.waiting.size() > 1) {
      auto highest = std::prev(global.waiting.end());
      auto rows = global.waiting_rows - highest->second.rows;
      auto bytes = global.waiting_bytes - highest->second.bytes;
      if (!fills(rows, bytes)) {
        break;
      }
      global.waiting_rows = rows;
      global.waiting_bytes = bytes;
      global.waiting.erase(highest);
    }
    if (fills(global.waiting_rows, global.waiting_bytes)) {
      auto highest = global.waiting.rbegin()->first;
      if (highest < global.head_cutoff) {
        global.head_cutoff = highest;
      }
    }
  }

  // The end of this thread's rows. Its head rows are passed on or left
  // waiting as at the end of a batch, except for the last thread: no thread
  // is left to take the rows that wait, so it passes all of them on. False
  // when the wrapped collector blocked.
  bool LeaveHead(ExecutionContext &context, TeeGlobalState &global, TeeLocalState &local,
                 InterruptState &interrupt) const {
    FlushStore(global, local);
    auto batches = BatchOrdered();
    if (!local.left) {
      local.left = true;
      lock_guard<mutex> guard(global.head_lock);
      auto last = --global.threads == 0;
      if (!batches) {
        if (last && global.head_inner && !global.passer) {
          // Nothing follows the chunks passed on so far: their state is combined.
          global.passer = &local;
          local.passer = true;
        }
      } else if (global.head_done) {
        DropHeld(local);
      } else if (last) {
        TakeAllWaiting(global, local);
      } else if (local.current_batch.IsValid()) {
        if (local.lowest || local.current_batch.GetIndex() <= PublishMinBatch(global, local)) {
          TakeWaitingBefore(global, local);
        } else {
          WaitLocked(global, local);
        }
      }
    }
    if (!batches) {
      return !local.passer || FinishPassing(context, global, local, interrupt);
    }
    if (local.last_batch.IsValid() && !global.head_done) {
      // The rows of later batches cannot be passed on as part of this
      // thread's batch: the wrapped collector is told of a batch after them.
      SyncPartition(global, local, local.last_batch);
      OperatorSinkNextBatchInput inner {*global.inner, *local.inner, interrupt};
      if (BASE::NextBatch(context, inner) == SinkNextBatchType::BLOCKED) {
        return false;
      }
      local.current_batch = local.last_batch;
    }
    local.last_batch = optional_idx();
    return PassHeld(context, global, local, interrupt);
  }

  // Requires head_lock. Every other thread has combined, so the batches that
  // wait and this thread's own are all that is left of the head.
  void TakeAllWaiting(TeeGlobalState &global, TeeLocalState &local) const {
    auto batch = local.partition_info.batch_index.GetIndex();
    auto last = batch;
    vector<TeeHeldChunk> all;
    auto own_placed = !local.current_batch.IsValid();
    auto place_own = [&]() {
      for (idx_t i = local.held_passed; i < local.held.size(); i++) {
        all.push_back(std::move(local.held[i]));
      }
      own_placed = true;
    };
    for (auto &entry : global.waiting) {
      if (!own_placed && entry.first > local.current_batch.GetIndex()) {
        place_own();
      }
      for (auto &held : entry.second.chunks) {
        all.push_back(std::move(held));
      }
      last = MaxValue(last, entry.first);
    }
    if (!own_placed) {
      place_own();
    }
    ClearWaiting(global);
    local.held = std::move(all);
    local.held_passed = 0;
    local.lowest = true;
    if (last != batch && !local.held.empty()) {
      local.last_batch = last;
    }
  }

  // Adds a chunk to the Arrow batch this thread is building and returns its
  // Arrow bytes. Rows are converted while they are stored, or while the head
  // still needs their size. A failure to store never fails the statement.
  idx_t Convert(TeeGlobalState &global, TeeLocalState &local, DataChunk &chunk) const {
    if (!global.storing && global.head_done) {
      return 0;
    }
    try {
      if (!local.batch) {
        local.batch = make_uniq<ArrowAppender>(parameters->types, BATCH_ROWS, properties, extension_types);
      }
      auto before = local.batch->ByteSize();
      local.batch->Append(chunk, 0, chunk.size(), chunk.size());
      return local.batch->ByteSize() - before;
    } catch (std::exception &) {
      local.batch.reset();
      global.storing = false;
      return 0;
    }
  }

  // Stores a chunk in this thread's place in the result.
  idx_t Store(TeeGlobalState &global, TeeLocalState &local, DataChunk &chunk) const {
    auto bytes = Convert(global, local, chunk);
    if (!local.batch) {
      ReportMissing(global, local);
    } else if (local.batch->RowCount() >= BATCH_ROWS || local.batch->ByteSize() >= BATCH_BYTES) {
      FlushStore(global, local);
    }
    return bytes;
  }

  // The key of the next batch this thread stores: executor batch, then the
  // thread's own count. Without executor batches: after the head, in the
  // order of arrival.
  void NextKey(TeeGlobalState &global, TeeLocalState &local, idx_t &major, idx_t &minor) const {
    if (BatchOrdered()) {
      major = InnerBatch(local).GetIndex();
      minor = local.pieces++;
    } else {
      major = 1;
      minor = global.arrivals++;
    }
  }

  void FlushStore(TeeGlobalState &global, TeeLocalState &local) const {
    if (!local.batch || local.batch->RowCount() == 0) {
      local.batch.reset();
      return;
    }
    idx_t major;
    idx_t minor;
    NextKey(global, local, major, minor);
    FlushStoreAs(global, local, major, minor);
  }

  // Stores the rows this thread converted as the batch with this key, or
  // tells the cache that the batch is the first without its rows.
  void FlushStoreAs(TeeGlobalState &global, TeeLocalState &local, idx_t major, idx_t minor) const {
    auto batch = std::move(local.batch);
    if (batch && batch->RowCount() > 0 && global.storing) {
      try {
        ArrowArrayWrapper array;
        array.arrow_array = batch->Finalize();
        if (sidequery_result_append(global.writer, &array.arrow_array, major, minor)) {
          return;
        }
        // The cache has recorded the batch as missing.
        global.storing = false;
        local.reported_missing = true;
        return;
      } catch (std::exception &) {
      }
    }
    global.storing = false;
    if (!local.reported_missing) {
      local.reported_missing = true;
      sidequery_result_missing(global.writer, major, minor);
    }
  }

  // Tells the cache that the next batch of this thread is the first of the
  // result without its rows.
  void ReportMissing(TeeGlobalState &global, TeeLocalState &local) const {
    global.storing = false;
    if (local.reported_missing) {
      return;
    }
    local.reported_missing = true;
    idx_t major;
    idx_t minor;
    NextKey(global, local, major, minor);
    sidequery_result_missing(global.writer, major, minor);
  }

  const shared_ptr<TeeParameters> parameters;
  const ClientProperties properties;
  const unordered_map<idx_t, const shared_ptr<ArrowTypeExtensionData>> extension_types;
};

static constexpr const char *TEE_STATEMENT = "sqlnb_result_tee";
static constexpr const char *TEE_PLACEMENT =
    "sqlnb_result_tee keeps the result of a SELECT statement that returns its columns unchanged: "
    "select * from sqlnb_result_tee(...)";

// Wraps the collector the statement would have had.
static unique_ptr<PhysicalOperator> TeeCollector(ClientContext &context, PreparedStatementData &data) {
  auto statement = context.registered_state->Get<TeeStatement>(TEE_STATEMENT);
  shared_ptr<TeeParameters> parameters;
  if (statement) {
    parameters = statement->parameters;
    statement->Unhook(context);
  }
  auto &previous = ClientConfig::GetConfig(context).get_result_collector;
  auto inner = previous ? previous(context, data) : PhysicalResultCollector::GetResultCollector(context, data);
  if (!parameters) {
    return inner;
  }
  // What is kept is the statement's result, in the schema checked at bind.
  if (data.statement_type != StatementType::SELECT_STATEMENT ||
      data.properties.return_type != StatementReturnType::QUERY_RESULT || data.types != parameters->types ||
      IdentifiersToStrings(data.names) != parameters->names) {
    throw InvalidInputException(TEE_PLACEMENT);
  }
  statement->collecting = true;
  auto &plan = *data.physical_plan;
  if (inner->Cast<PhysicalResultCollector>().BuildsOwnResult()) {
    return make_uniq<TeeSink<DelegatingCollector>>(context, std::move(parameters), plan, data, std::move(inner));
  }
  // DuckDB's own collector is handed its result buffer after this returns, so
  // the tee has to be that collector.
  auto ordering = inner->Cast<PhysicalResultSink>().ordering;
  return make_uniq<TeeSink<PhysicalResultSink>>(context, std::move(parameters), plan, data, ordering);
}

static unique_ptr<FunctionData> TeeBind(ClientContext &context, TableFunctionBindInput &input,
                                        vector<LogicalType> &return_types, vector<Identifier> &names) {
  RejectAgentConnection(context, "sqlnb_result_tee");
  auto parameters = make_shared_ptr<TeeParameters>();
  parameters->id = NamedParameter(input, "id").ToString();
  parameters->meta = NamedParameter(input, "meta").ToString();
  parameters->head_rows = CountParameter(input, "head_rows");
  parameters->head_bytes = CountParameter(input, "head_bytes");
  parameters->max_result_bytes = CountParameter(input, "max_result_bytes");
  parameters->max_cache_bytes = CountParameter(input, "max_cache_bytes");
  auto &ttl_seconds = input.named_parameters.at("ttl_seconds");
  if (ttl_seconds.IsNull()) {
    parameters->ttl_seconds = 0;
  } else if (ttl_seconds.GetValue<int64_t>() <= 0) {
    throw InvalidInputException("ttl_seconds must be positive, or NULL to keep the result until DuckDB exits");
  } else {
    parameters->ttl_seconds = NumericCast<idx_t>(ttl_seconds.GetValue<int64_t>());
  }
  parameters->stop_at_limit = NamedParameter(input, "stop_at_limit").GetValue<bool>();
  parameters->types = input.input_table_types;
  parameters->names = IdentifiersToStrings(input.input_table_names);
  // Reject unsupported columns and taken ids before anything executes.
  ArrowSchemaWrapper schema;
  ExportSchema(context, parameters->types, parameters->names, schema.arrow_schema);
  char *error = nullptr;
  if (!sidequery_result_check(parameters->id.c_str(), parameters->meta.c_str(), &schema.arrow_schema, &error)) {
    ThrowCacheError(error);
  }
  auto statement = context.registered_state->GetOrCreate<TeeStatement>(TEE_STATEMENT);
  if (statement->parameters && statement->parameters->id != parameters->id) {
    throw InvalidInputException("a statement keeps one result: sqlnb_result_tee appears twice");
  }
  statement->parameters = parameters;
  if (!statement->hooked && !statement->collecting) {
    // The statement's collector is created after its plan. Until then the
    // hook is this function's; the collector it replaces is asked first.
    auto &config = ClientConfig::GetConfig(context);
    statement->previous = config.get_result_collector;
    config.get_result_collector = tee_collector_t(TeeCollector);
    statement->hooked = true;
  }
  auto data = make_uniq<TeeBindData>();
  data->parameters = std::move(parameters);
  return_types = input.input_table_types;
  names = input.input_table_names;
  return std::move(data);
}

// The function itself passes its input through on every thread; the
// statement's collector does the storing.
class TeeFunctionState : public GlobalTableFunctionState {
public:
  idx_t MaxThreads() const override {
    return MAX_THREADS;
  }
};

static unique_ptr<GlobalTableFunctionState> TeeInit(ClientContext &context, TableFunctionInitInput &input) {
  auto statement = context.registered_state->Get<TeeStatement>(TEE_STATEMENT);
  auto &parameters = input.bind_data->Cast<TeeBindData>().parameters;
  if (!statement || !statement->collecting || !statement->parameters || statement->parameters->id != parameters->id) {
    // Without the collector the rows would pass through and never be kept.
    throw InvalidInputException(TEE_PLACEMENT);
  }
  return make_uniq<TeeFunctionState>();
}

static OperatorResultType TeeExecute(ExecutionContext &, TableFunctionInput &, DataChunk &input, DataChunk &output) {
  output.Reference(input);
  return OperatorResultType::NEED_MORE_INPUT;
}

static unique_ptr<FunctionData> BindArrowScan(ClientContext &context, shared_ptr<ArrowScanFactory> factory,
                                              bool projection_pushdown, vector<LogicalType> &return_types,
                                              vector<Identifier> &names) {
  auto data = make_uniq<ArrowScanFunctionData>(std::move(factory));
  data->factory->GetSchema(data->schema_root.arrow_schema);
  ArrowTableFunction::PopulateArrowTableSchema(context, data->arrow_table, data->schema_root.arrow_schema);
  names = StringsToIdentifiers(data->arrow_table.GetNames());
  return_types = data->arrow_table.GetTypes();
  data->all_types = return_types;
  data->projection_pushdown_enabled = projection_pushdown;
  return std::move(data);
}

class StoredResultFactory : public ArrowScanFactory {
public:
  StoredResultFactory(string id_p, idx_t first_row_p, idx_t row_count_p, Value row_number_p)
      : id(std::move(id_p)), first_row(first_row_p), row_count(row_count_p), has_row_number(!row_number_p.IsNull()),
        row_number(has_row_number ? row_number_p.ToString() : string()) {
  }

  void GetSchema(ArrowSchema &schema) override {
    char *error = nullptr;
    if (!sidequery_result_schema(id.c_str(), RowNumber(), &schema, &error)) {
      ThrowCacheError(error);
    }
  }

  unique_ptr<ArrowArrayStreamWrapper> ProduceStream(ArrowStreamParameters &parameters) override {
    // The scan expects the projected columns in the order of its column ids.
    auto &projection = parameters.projected_columns.filter_to_col;
    vector<uint64_t> columns;
    for (idx_t position = 0; position < projection.size(); position++) {
      columns.push_back(projection.at(position));
    }
    auto stream = make_uniq<ArrowArrayStreamWrapper>();
    char *error = nullptr;
    if (!sidequery_result_scan(id.c_str(), first_row, row_count, RowNumber(), columns.data(), columns.size(),
                               &stream->arrow_array_stream, &error)) {
      ThrowCacheError(error);
    }
    return stream;
  }

private:
  const char *RowNumber() const {
    return has_row_number ? row_number.c_str() : nullptr;
  }

  const string id;
  const idx_t first_row;
  const idx_t row_count;
  const bool has_row_number;
  const string row_number;
};

static unique_ptr<FunctionData> StoredResultBind(ClientContext &context, TableFunctionBindInput &input,
                                                 vector<LogicalType> &return_types, vector<Identifier> &names) {
  RejectAgentConnection(context, "sqlnb_result");
  if (input.inputs[0].IsNull()) {
    throw InvalidInputException("id must not be NULL");
  }
  auto first_row = CountParameter(input, "first_row");
  auto &row_count = input.named_parameters.at("row_count");
  if (!row_count.IsNull() && row_count.GetValue<int64_t>() < 0) {
    throw InvalidInputException("row_count must not be negative");
  }
  auto factory = make_shared_ptr<StoredResultFactory>(
      input.inputs[0].ToString(), first_row,
      row_count.IsNull() ? NumericLimits<idx_t>::Maximum() : NumericCast<idx_t>(row_count.GetValue<int64_t>()),
      input.named_parameters.at("row_number"));
  return BindArrowScan(context, std::move(factory), true, return_types, names);
}

// Mirrors DuckDB's Arrow scan, so that a parallel read keeps the stored order.
static OperatorPartitionData StoredResultPartition(ClientContext &, TableFunctionGetPartitionInput &input) {
  if (input.partition_info.RequiresPartitionColumns()) {
    throw InternalException("sqlnb_result does not provide partition columns");
  }
  return OperatorPartitionData(input.local_state->Cast<ArrowScanLocalState>().batch_index);
}

class ResultListFactory : public ArrowScanFactory {
public:
  void GetSchema(ArrowSchema &schema) override {
    auto stream = ProduceList();
    if (stream->arrow_array_stream.get_schema(&stream->arrow_array_stream, &schema) != 0) {
      throw IOException("failed to read the result list schema");
    }
  }

  unique_ptr<ArrowArrayStreamWrapper> ProduceStream(ArrowStreamParameters &) override {
    return ProduceList();
  }

private:
  static unique_ptr<ArrowArrayStreamWrapper> ProduceList() {
    auto stream = make_uniq<ArrowArrayStreamWrapper>();
    char *error = nullptr;
    if (!sidequery_results_list(&stream->arrow_array_stream, &error)) {
      ThrowCacheError(error);
    }
    return stream;
  }
};

static unique_ptr<FunctionData> ResultListBind(ClientContext &context, TableFunctionBindInput &,
                                               vector<LogicalType> &return_types, vector<Identifier> &names) {
  RejectAgentConnection(context, "sqlnb_results");
  return BindArrowScan(context, make_shared_ptr<ResultListFactory>(), false, return_types, names);
}

static void DropResult(DataChunk &args, ExpressionState &state, Vector &result) {
  RejectAgentConnection(state.GetContext(), "sqlnb_result_drop");
  UnaryExecutor::Execute<string_t, bool>(args.data[0], result, args.size(), [](string_t id) {
    return sidequery_result_drop(id.GetString().c_str());
  });
}

} // namespace sqlnb_results

static void RegisterSQLNBResults(ExtensionLoader &loader) {
  using namespace sqlnb_results;
  // The storage limits a caller does not name are the cache's own.
  uint64_t max_result_bytes, max_cache_bytes, ttl_seconds;
  sidequery_result_defaults(&max_result_bytes, &max_cache_bytes, &ttl_seconds);
  TableFunction tee("sqlnb_result_tee",
                    FunctionSignature()
                        .AddPositionalOnly("input", LogicalType::TABLE)
                        .AddKeywordOnly("id", LogicalType::VARCHAR)
                        .AddKeywordOnly("head_rows", LogicalType::BIGINT, Value::BIGINT(DEFAULT_HEAD_ROWS))
                        .AddKeywordOnly("head_bytes", LogicalType::BIGINT, Value::BIGINT(DEFAULT_HEAD_BYTES))
                        .AddKeywordOnly("meta", LogicalType::VARCHAR, Value(""))
                        .AddKeywordOnly("max_result_bytes", LogicalType::BIGINT,
                                        Value::BIGINT(NumericCast<int64_t>(max_result_bytes)))
                        .AddKeywordOnly("max_cache_bytes", LogicalType::BIGINT,
                                        Value::BIGINT(NumericCast<int64_t>(max_cache_bytes)))
                        .AddKeywordOnly("ttl_seconds", LogicalType::BIGINT,
                                        Value::BIGINT(NumericCast<int64_t>(ttl_seconds)))
                        .AddKeywordOnly("stop_at_limit", LogicalType::BOOLEAN, Value::BOOLEAN(false)),
                    nullptr, TeeBind, TeeInit);
  tee.in_out_function = TeeExecute;
  loader.RegisterFunction(tee);

  TableFunction results("sqlnb_results", {}, ArrowTableFunction::ArrowScanFunction, ResultListBind,
                        ArrowTableFunction::ArrowScanInitGlobal, ArrowTableFunction::ArrowScanInitLocal);
  results.parallelism = TableFunctionParallelism::SEQUENTIAL;
  loader.RegisterFunction(results);

  TableFunction result("sqlnb_result",
                       FunctionSignature()
                           .AddParameter("id", LogicalType::VARCHAR)
                           .AddKeywordOnly("first_row", LogicalType::BIGINT, Value::BIGINT(0))
                           .AddKeywordOnly("row_count", LogicalType::BIGINT, Value(LogicalType::BIGINT))
                           .AddKeywordOnly("row_number", LogicalType::VARCHAR, Value(LogicalType::VARCHAR)),
                       ArrowTableFunction::ArrowScanFunction, StoredResultBind,
                       ArrowTableFunction::ArrowScanInitGlobal, ArrowTableFunction::ArrowScanInitLocal);
  result.projection_pushdown = true;
  result.get_partition_data = StoredResultPartition;
  result.parallelism = TableFunctionParallelism::SEQUENTIAL;
  loader.RegisterFunction(result);

  ScalarFunction drop("sqlnb_result_drop", {LogicalType::VARCHAR}, LogicalType::BOOLEAN, DropResult);
  drop.SetVolatile();
  drop.SetFallible();
  loader.RegisterFunction(drop);
}

} // namespace duckdb
#endif
