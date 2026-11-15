#pragma once

// Result cache (docs/results.md). sqlnb_result_tee passes a statement's first
// rows through while the Rust library stores every row as Arrow; the other
// functions list, read back and drop stored results through DuckDB's Arrow scan.
#if __has_include("duckdb/main/query_result_stream.hpp")
#include "duckdb/common/arrow/arrow_appender.hpp"
#include "duckdb/common/arrow/arrow_converter.hpp"
#include "duckdb/common/arrow/arrow_wrapper.hpp"
#include "duckdb/common/type_visitor.hpp"
#include "duckdb/common/vector_operations/unary_executor.hpp"
#include "duckdb/function/scalar_function.hpp"
#include "duckdb/function/table/arrow.hpp"
#include "duckdb/function/table/arrow/arrow_duck_schema.hpp"
#include "duckdb/function/table_function.hpp"
#include "duckdb/main/client_context.hpp"
#include "duckdb/main/client_context_state.hpp"
#include "duckdb/main/config.hpp"
#include "duckdb/main/extension/extension_loader.hpp"

namespace duckdb {
namespace sqlnb_results {

static constexpr int64_t DEFAULT_HEAD_ROWS = 10240;
static constexpr int64_t DEFAULT_HEAD_BYTES = 4LL * 1024 * 1024;
static constexpr int64_t DEFAULT_MAX_RESULT_BYTES = 4LL * 1024 * 1024 * 1024;
static constexpr int64_t DEFAULT_MAX_CACHE_BYTES = 16LL * 1024 * 1024 * 1024;
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

struct TeeBindData : public TableFunctionData {
  string id;
  string meta;
  idx_t head_rows;
  idx_t head_bytes;
  idx_t max_result_bytes;
  idx_t max_cache_bytes;
  bool stop_at_limit;
  vector<LogicalType> types;
  vector<string> names;
};

static unique_ptr<FunctionData> TeeBind(ClientContext &context, TableFunctionBindInput &input,
                                        vector<LogicalType> &return_types, vector<Identifier> &names) {
  RejectAgentConnection(context, "sqlnb_result_tee");
  auto data = make_uniq<TeeBindData>();
  data->id = NamedParameter(input, "id").ToString();
  data->meta = NamedParameter(input, "meta").ToString();
  data->head_rows = CountParameter(input, "head_rows");
  data->head_bytes = CountParameter(input, "head_bytes");
  data->max_result_bytes = CountParameter(input, "max_result_bytes");
  data->max_cache_bytes = CountParameter(input, "max_cache_bytes");
  data->stop_at_limit = NamedParameter(input, "stop_at_limit").GetValue<bool>();
  data->types = input.input_table_types;
  data->names = IdentifiersToStrings(input.input_table_names);
  // Reject unsupported columns and taken ids before anything executes.
  ArrowSchemaWrapper schema;
  ExportSchema(context, data->types, data->names, schema.arrow_schema);
  char *error = nullptr;
  if (!sidequery_result_check(data->id.c_str(), data->meta.c_str(), &schema.arrow_schema, &error)) {
    ThrowCacheError(error);
  }
  return_types = input.input_table_types;
  names = input.input_table_names;
  return std::move(data);
}

// Passes the first rows of the input through and stores every row. Rows leave
// in full chunks so that Quack can send them before the statement ends.
class TeeState : public GlobalTableFunctionState {
public:
  TeeState(ClientContext &context, const TeeBindData &bind)
      : types(bind.types), properties(StorageProperties(context)),
        extension_types(ArrowTypeExtensionData::GetExtensionTypes(context, bind.types)),
        head_rows(bind.head_rows), head_bytes(bind.head_bytes), stop_at_limit(bind.stop_at_limit),
        head_open(bind.head_rows > 0 && bind.head_bytes > 0) {
    ArrowSchemaWrapper schema;
    ExportSchema(context, types, bind.names, schema.arrow_schema);
    char *error = nullptr;
    writer = sidequery_result_begin(bind.id.c_str(), bind.meta.c_str(), &schema.arrow_schema, bind.head_rows,
                                    bind.max_result_bytes, bind.max_cache_bytes, &error);
    if (!writer) {
      ThrowCacheError(error);
    }
    pending.Initialize(context, types);
  }

  // An interrupted or failed statement never finishes its result.
  ~TeeState() override {
    if (writer) {
      sidequery_result_abort(writer);
    }
  }

  // One thread runs the pipeline, so rows reach the cache in the statement's order.
  idx_t MaxThreads() const override {
    return 1;
  }

  OperatorResultType Consume(DataChunk &input, DataChunk &output) {
    while (input_offset < input.size() && !StoppedAtLimit()) {
      if (!head_open) {
        Store(input, input_offset, input.size());
        rows += input.size() - input_offset;
        input_offset = input.size();
        break;
      }
      auto chunk_rows = MinValue<idx_t>(STANDARD_VECTOR_SIZE, head_rows - returned_rows);
      auto take = MinValue<idx_t>(input.size() - input_offset, chunk_rows - pending.size());
      pending.Append(input, SelectionVector(input_offset, take), take);
      input_offset += take;
      rows += take;
      if (pending.size() == chunk_rows) {
        EmitHead(output);
        if (input_offset < input.size() && !StoppedAtLimit()) {
          return OperatorResultType::HAVE_MORE_OUTPUT;
        }
      }
    }
    if (StoppedAtLimit()) {
      if (output.size() > 0) {
        // DuckDB drops the rows of a call that returns FINISHED; finish on the next call.
        return OperatorResultType::HAVE_MORE_OUTPUT;
      }
      // Stop reading, as a LIMIT would, and publish what was stored. DuckDB does
      // not call the final function of an operator that finished early.
      input_offset = 0;
      Complete(false);
      return OperatorResultType::FINISHED;
    }
    input_offset = 0;
    return OperatorResultType::NEED_MORE_INPUT;
  }

  void Finish(DataChunk &output) {
    if (!writer) {
      return;
    }
    if (head_open && pending.size() > 0) {
      // The whole input was shorter than the head.
      EmitHead(output);
    }
    Complete(true);
  }

private:
  bool StoppedAtLimit() const {
    return stop_at_limit && !storing;
  }

  void EmitHead(DataChunk &output) {
    auto bytes = Store(pending, 0, pending.size());
    output.Append(pending);
    returned_rows += pending.size();
    returned_bytes += bytes;
    pending.Reset();
    if (returned_rows >= head_rows || returned_bytes >= head_bytes) {
      head_open = false;
    }
  }

  // Converts rows into the batch being built and returns their Arrow bytes.
  // Rows are converted while storing, or while the head still needs their size.
  idx_t Store(DataChunk &chunk, idx_t from, idx_t to) {
    if (!storing && !head_open) {
      return 0;
    }
    try {
      if (!batch) {
        batch = make_uniq<ArrowAppender>(types, BATCH_ROWS, properties, extension_types);
      }
      auto before = batch->ByteSize();
      batch->Append(chunk, from, to, chunk.size());
      auto bytes = batch->ByteSize() - before;
      if (batch->RowCount() >= BATCH_ROWS || batch->ByteSize() >= BATCH_BYTES) {
        Flush();
      }
      return bytes;
    } catch (std::exception &) {
      // A failure to store never fails the statement.
      batch.reset();
      StopStoring();
      return 0;
    }
  }

  void Flush() {
    if (!batch || batch->RowCount() == 0) {
      return;
    }
    ArrowArrayWrapper array;
    array.arrow_array = batch->Finalize();
    batch.reset();
    if (storing) {
      storing = sidequery_result_append(writer, &array.arrow_array);
    }
  }

  void StopStoring() {
    if (storing) {
      storing = false;
      sidequery_result_stop(writer);
    }
  }

  void Complete(bool rows_exact) {
    try {
      Flush();
    } catch (std::exception &) {
      StopStoring();
    }
    sidequery_result_finish(writer, rows, rows_exact);
    writer = nullptr;
  }

  sidequery_result_writer *writer = nullptr;
  const vector<LogicalType> types;
  const ClientProperties properties;
  const unordered_map<idx_t, const shared_ptr<ArrowTypeExtensionData>> extension_types;
  const idx_t head_rows;
  const idx_t head_bytes;
  const bool stop_at_limit;
  // Rows of the head that do not fill a chunk yet.
  DataChunk pending;
  unique_ptr<ArrowAppender> batch;
  bool head_open;
  bool storing = true;
  idx_t input_offset = 0;
  idx_t rows = 0;
  idx_t returned_rows = 0;
  idx_t returned_bytes = 0;
};

static unique_ptr<GlobalTableFunctionState> TeeInit(ClientContext &context, TableFunctionInitInput &input) {
  return make_uniq<TeeState>(context, input.bind_data->Cast<TeeBindData>());
}

static OperatorResultType TeeExecute(ExecutionContext &, TableFunctionInput &data, DataChunk &input,
                                     DataChunk &output) {
  return data.global_state->Cast<TeeState>().Consume(input, output);
}

static OperatorFinalizeResultType TeeFinal(ExecutionContext &, TableFunctionInput &data, DataChunk &output) {
  data.global_state->Cast<TeeState>().Finish(output);
  return OperatorFinalizeResultType::FINISHED;
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
  TableFunction tee("sqlnb_result_tee",
                    FunctionSignature()
                        .AddPositionalOnly("input", LogicalType::TABLE)
                        .AddKeywordOnly("id", LogicalType::VARCHAR)
                        .AddKeywordOnly("head_rows", LogicalType::BIGINT, Value::BIGINT(DEFAULT_HEAD_ROWS))
                        .AddKeywordOnly("head_bytes", LogicalType::BIGINT, Value::BIGINT(DEFAULT_HEAD_BYTES))
                        .AddKeywordOnly("meta", LogicalType::VARCHAR, Value(""))
                        .AddKeywordOnly("max_result_bytes", LogicalType::BIGINT,
                                        Value::BIGINT(DEFAULT_MAX_RESULT_BYTES))
                        .AddKeywordOnly("max_cache_bytes", LogicalType::BIGINT,
                                        Value::BIGINT(DEFAULT_MAX_CACHE_BYTES))
                        .AddKeywordOnly("stop_at_limit", LogicalType::BOOLEAN, Value::BOOLEAN(false)),
                    nullptr, TeeBind, TeeInit);
  tee.in_out_function = TeeExecute;
  tee.in_out_function_final = TeeFinal;
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
