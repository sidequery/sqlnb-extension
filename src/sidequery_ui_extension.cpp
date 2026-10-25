// Include the platform declarations before DuckDB defines its restrict macro.
#ifndef _WIN32
#include <spawn.h>
#include <sys/wait.h>
#endif

#include "sidequery_ui_extension.hpp"
#include "sidequery_http.h"

#include "duckdb/main/connection.hpp"
#include "duckdb/main/client_context_state.hpp"
#include "duckdb/main/database.hpp"
#include "duckdb/main/extension/extension_loader.hpp"
#include "duckdb/main/extension_helper.hpp"
#include "duckdb/main/settings.hpp"
#include "duckdb/main/prepared_statement.hpp"
#include "duckdb/catalog/catalog.hpp"
#include "duckdb/catalog/entry_lookup_info.hpp"
#include "duckdb/parser/expression/function_expression.hpp"
#include "duckdb/parser/expression/subquery_expression.hpp"
#include "duckdb/parser/parsed_expression_iterator.hpp"
#include "duckdb/parser/statement/select_statement.hpp"
#include "duckdb/parser/tableref/table_function_ref.hpp"
#include "duckdb/parser/tableref/basetableref.hpp"
#include "duckdb/planner/logical_operator_visitor.hpp"
#include "duckdb/planner/operator/logical_get.hpp"
#include "duckdb/storage/storage_extension.hpp"

#if __has_include("duckdb/main/query_result_stream.hpp")
#include "duckdb/main/query_result_stream.hpp"
#define SQLNB_HAS_RESULT_STREAM 1
#endif

#if __has_include("duckdb/main/extension_repository_manager.hpp")
#include "duckdb/main/extension_repository_manager.hpp"
#define SQLNB_HAS_USER_EXTENSION_REPOSITORIES 1
#endif

#include <openssl/rand.h>

#include <cerrno>
#include <cmath>
#include <cstring>
#include <iomanip>
#include <memory>
#include <mutex>
#include <sstream>
#include <thread>

#ifndef _WIN32
extern char **environ;
#endif
#ifdef _WIN32
#include <shellapi.h>
#include <windows.h>
#endif

namespace duckdb {
namespace {

static constexpr const char *DEFAULT_SQLNB_UI = "https://app.sidequery.dev/sqlnb/";
#ifdef SQLNB_HAS_USER_EXTENSION_REPOSITORIES
static constexpr const char *SIDEQUERY_REPOSITORY = "sidequery";
#endif

static string JSONEscape(const string &value) {
  std::ostringstream out;
  out << '"';
  for (auto byte : value) {
    switch (byte) {
    case '"': out << "\\\""; break;
    case '\\': out << "\\\\"; break;
    case '\b': out << "\\b"; break;
    case '\f': out << "\\f"; break;
    case '\n': out << "\\n"; break;
    case '\r': out << "\\r"; break;
    case '\t': out << "\\t"; break;
    default:
      if (static_cast<unsigned char>(byte) < 0x20) {
        out << "\\u" << std::hex << std::setw(4) << std::setfill('0')
            << static_cast<unsigned>(static_cast<unsigned char>(byte)) << std::dec;
      } else {
        out << byte;
      }
    }
  }
  out << '"';
  return out.str();
}

static string RandomHex(idx_t bytes) {
  vector<unsigned char> random(bytes);
  if (RAND_bytes(random.data(), NumericCast<int>(random.size())) != 1) {
    throw IOException("failed to generate a SQLNB capability");
  }
  std::ostringstream out;
  out << std::hex << std::setfill('0');
  for (auto byte : random) {
    out << std::setw(2) << static_cast<unsigned>(byte);
  }
  return out.str();
}

static void LoadQuack(DatabaseInstance &database) {
  if (database.ExtensionIsLoaded("quack")) {
    return;
  }
#ifdef SQLNB_HAS_USER_EXTENSION_REPOSITORIES
  auto &file_system = FileSystem::GetFileSystem(database);
  ExtensionRepository repository;
  if (ExtensionRepositoryManager::TryGetRepository(database, file_system, SIDEQUERY_REPOSITORY,
                                                    repository)) {
    ExtensionInstallOptions install_options;
    install_options.repository = &repository;
    install_options.throw_on_origin_mismatch = true;
    ExtensionHelper::InstallExtension(database, file_system, "quack", install_options);

    ExtensionLoadOptions load_options("quack");
    load_options.repository = repository.name;
    ExtensionHelper::LoadExternalExtension(database, file_system, load_options);
    return;
  }
#endif
  ExtensionHelper::AutoLoadExtension(database, "quack");
}

static void EnsureDependenciesLoaded(DatabaseInstance &database) {
  // Source-column date profiles use timezone conversion. Load ICU before
  // accepting notebook queries so their first date query never installs it.
  if (!database.ExtensionIsLoaded("icu")) {
    ExtensionHelper::AutoLoadExtension(database, "icu");
  }
  if (!database.ExtensionIsLoaded("icu")) {
    throw MissingExtensionException("SQLNB requires the DuckDB icu extension for date profiles");
  }
  // Quack uses DuckDB's writable crypto provider to generate session IDs.
  // For DuckDB 1.5.4 that provider is installed by the matching httpfs
  // extension; this is also an explicit dependency in Quack's own config.
  if (!database.ExtensionIsLoaded("httpfs")) {
    ExtensionHelper::AutoLoadExtension(database, "httpfs");
  }
  if (!database.ExtensionIsLoaded("httpfs")) {
    throw MissingExtensionException("SQLNB requires the DuckDB httpfs extension for Quack sessions");
  }
  if (!database.ExtensionIsLoaded("quack")) {
    LoadQuack(database);
  }
  if (!database.ExtensionIsLoaded("quack")) {
    throw MissingExtensionException(
        "SQLNB requires DuckDB Quack; install it from the same Sidequery extension repository");
  }
}

struct QuackEndpoint {
  string listen_uri;
  string listen_url;
  string token;
};

static QuackEndpoint StartQuack(DatabaseInstance &database) {
  EnsureDependenciesLoaded(database);
  auto port = sidequery_http_unused_port();
  if (port == 0) {
    throw IOException("failed to reserve a loopback port for DuckDB Quack");
  }
  auto requested_uri = "quack:127.0.0.1:" + std::to_string(port);
  auto token = RandomHex(32);
  Connection connection(database);
  auto result = connection.Query("from quack_serve(?, token = ?)", Value(requested_uri), Value(token));
  if (!result || result->HasError()) {
    throw IOException("failed to start DuckDB Quack: %s", result ? result->GetError() : "no result");
  }
  auto chunk = result->Fetch();
  if (!chunk || chunk->size() != 1 || chunk->ColumnCount() < 3) {
    throw IOException("DuckDB Quack returned an invalid startup result");
  }
  return {chunk->GetValue(0, 0).ToString(), chunk->GetValue(1, 0).ToString(),
          chunk->GetValue(2, 0).ToString()};
}

static void StopQuack(DatabaseInstance &database, const string &listen_uri) {
  if (listen_uri.empty() || !database.ExtensionIsLoaded("quack")) {
    return;
  }
  Connection connection(database);
  auto result = connection.Query("from quack_stop(?)", Value(listen_uri));
  if (!result || result->HasError()) {
    throw IOException("failed to stop DuckDB Quack: %s", result ? result->GetError() : "no result");
  }
}

// The owner connection's cleanup hook releases this connection, avoiding a
// database -> extension state -> agent connection -> database reference cycle.
class McpConnection {
public:
  explicit McpConnection(DatabaseInstance &database) : connection(database) {}

  static char *Query(void *context, uint64_t id, const char *sql, uint64_t max_rows,
                     uint64_t max_bytes, bool read_only) noexcept {
    auto &self = *static_cast<McpConnection *>(context);
    std::lock_guard<std::mutex> serial(self.query_lock);
    {
      std::lock_guard<std::mutex> guard(self.active_lock);
      self.active_id = id;
      self.cancelled = false;
    }
    string response;
    try {
      response = self.Execute(sql, max_rows, max_bytes, read_only);
    } catch (const std::exception &error) {
      string message = error.what();
      auto policy = message.find("POLICY_BLOCKED:");
      if (policy != string::npos) {
        // DuckDB prefixes InvalidInputException messages with its error class.
        // Preserve the MCP contract's stable code at the response boundary.
        message = message.substr(policy);
      }
      response = "{\"error\":" + JSONEscape(message) + "}";
    } catch (...) {
      response = "{\"error\":\"unexpected DuckDB query failure\"}";
    }
    {
      std::lock_guard<std::mutex> guard(self.active_lock);
      if (self.cancelled) {
        response = "{\"error\":\"query cancelled\"}";
      }
      self.active_id = 0;
    }
    auto result = static_cast<char *>(std::malloc(response.size() + 1));
    if (result) {
      std::memcpy(result, response.c_str(), response.size() + 1);
    }
    return result;
  }

  static void Interrupt(void *context, uint64_t id) noexcept {
    auto &self = *static_cast<McpConnection *>(context);
    std::lock_guard<std::mutex> guard(self.active_lock);
    if (id != 0 && self.active_id == id) {
      self.cancelled = true;
      self.connection.Interrupt();
    }
  }

  static void Free(char *result) { std::free(result); }

private:
  static string JSONValue(const Value &value) {
    if (value.IsNull()) {
      return "null";
    }
    switch (value.type().id()) {
    case LogicalTypeId::BOOLEAN:
      return value.GetValue<bool>() ? "true" : "false";
    case LogicalTypeId::TINYINT:
    case LogicalTypeId::SMALLINT:
    case LogicalTypeId::INTEGER:
    case LogicalTypeId::UTINYINT:
    case LogicalTypeId::USMALLINT:
    case LogicalTypeId::UINTEGER:
      return value.ToString();
    case LogicalTypeId::FLOAT:
    case LogicalTypeId::DOUBLE:
      if (std::isfinite(value.GetValue<double>())) {
        return value.ToString();
      }
      break;
    default:
      break;
    }
    // Preserve BIGINT, DECIMAL, dates and nested values without JSON precision loss.
    return JSONEscape(value.ToString());
  }

  static bool StateChangingFunction(const string &name) {
    static const unordered_set<string> blocked = {
        "nextval", "setval", "write_csv", "write_json", "write_parquet", "checkpoint",
        "force_checkpoint", "query", "query_table", "http_post", "http_put", "http_delete"};
    return blocked.count(name) || StringUtil::StartsWith(name, "quack_") ||
           StringUtil::StartsWith(name, "sqlnb_") || StringUtil::StartsWith(name, "sidequery_ui_");
  }

  void CheckReadOnlyExpression(ParsedExpression &expression) {
    if (expression.GetExpressionClass() == ExpressionClass::FUNCTION) {
      auto &function = expression.Cast<FunctionExpression>();
#ifdef SQLNB_HAS_USER_EXTENSION_REPOSITORIES
      auto name = StringUtil::Lower(function.FunctionName().GetIdentifierName());
#else
      auto name = StringUtil::Lower(function.function_name);
#endif
      // User macros can hide functions that perform work while binding, before
      // DuckDB has produced the statement properties or logical plan to audit.
#ifdef SQLNB_HAS_USER_EXTENSION_REPOSITORIES
      auto entry = Catalog::GetEntry(*connection.context,
          EntryLookupInfo(CatalogType::SCALAR_FUNCTION_ENTRY, function.GetQualifiedName()),
          OnEntryNotFound::RETURN_NULL);
#else
      auto entry = Catalog::GetEntry(*connection.context, CatalogType::SCALAR_FUNCTION_ENTRY,
          function.catalog, function.schema, function.function_name, OnEntryNotFound::RETURN_NULL);
#endif
      if (entry && entry->type == CatalogType::MACRO_ENTRY && !entry->internal) {
        throw InvalidInputException("POLICY_BLOCKED: sidequery.sql.query cannot verify user-defined macros before binding; use sidequery.sql.execute");
      }
      if (StateChangingFunction(name)) {
        throw InvalidInputException("POLICY_BLOCKED: sidequery.sql.query rejects state-changing function '%s'; use sidequery.sql.execute", name);
      }
    }
    if (expression.GetExpressionClass() == ExpressionClass::SUBQUERY) {
      auto &subquery = expression.Cast<SubqueryExpression>();
#ifdef SQLNB_HAS_USER_EXTENSION_REPOSITORIES
      CheckReadOnlyNode(*subquery.SubqueryMutable()->node);
#else
      CheckReadOnlyNode(*subquery.subquery->node);
#endif
    }
    ParsedExpressionIterator::EnumerateChildren(expression, [this](ParsedExpression &child) {
      CheckReadOnlyExpression(child);
    });
  }

  void CheckReadOnlyNode(QueryNode &node) {
    ParsedExpressionIterator::EnumerateQueryNodeChildren(node, [this](unique_ptr<ParsedExpression> &expression) {
      CheckReadOnlyExpression(*expression);
    }, [this](TableRef &reference) {
      if (reference.type == TableReferenceType::BASE_TABLE) {
        auto &table = reference.Cast<BaseTableRef>();
#ifdef SQLNB_HAS_USER_EXTENSION_REPOSITORIES
        auto entry = Catalog::GetEntry(*connection.context,
            EntryLookupInfo(CatalogType::TABLE_ENTRY, table.GetQualifiedName()),
            OnEntryNotFound::RETURN_NULL);
#else
        auto entry = Catalog::GetEntry(*connection.context, CatalogType::TABLE_ENTRY,
            table.catalog_name, table.schema_name, table.table_name, OnEntryNotFound::RETURN_NULL);
#endif
        if (entry && entry->type == CatalogType::VIEW_ENTRY && !entry->internal) {
          throw InvalidInputException("POLICY_BLOCKED: sidequery.sql.query cannot verify user-defined views before binding; use sidequery.sql.execute");
        }
      }
      if (reference.type == TableReferenceType::TABLE_FUNCTION) {
        auto &table = reference.Cast<TableFunctionRef>();
        auto &function = table.function->Cast<FunctionExpression>();
#ifdef SQLNB_HAS_USER_EXTENSION_REPOSITORIES
        auto entry = Catalog::GetEntry(*connection.context,
            EntryLookupInfo(CatalogType::TABLE_FUNCTION_ENTRY, function.GetQualifiedName()),
            OnEntryNotFound::RETURN_NULL);
        CheckReadOnlyScan(StringUtil::Lower(function.FunctionName().GetIdentifierName()));
#else
        auto entry = Catalog::GetEntry(*connection.context, CatalogType::TABLE_FUNCTION_ENTRY,
            function.catalog, function.schema, function.function_name, OnEntryNotFound::RETURN_NULL);
        CheckReadOnlyScan(StringUtil::Lower(function.function_name));
#endif
        if (entry && entry->type == CatalogType::TABLE_MACRO_ENTRY && !entry->internal) {
          throw InvalidInputException("POLICY_BLOCKED: sidequery.sql.query cannot verify user-defined macros before binding; use sidequery.sql.execute");
        }
        if (table.subquery) {
          CheckReadOnlyNode(*table.subquery->node);
        }
      }
    });
  }

  static void CheckReadOnlyScan(const string &name) {
    static const unordered_set<string> read_scans = {
        "seq_scan", "table_scan", "range", "generate_series", "unnest", "repeat",
        "read_csv", "read_csv_auto", "csv_scan", "read_parquet", "parquet_scan",
        "read_json", "read_json_auto", "read_ndjson", "read_ndjson_auto", "json_scan",
        "read_json_objects", "read_json_objects_auto", "read_ndjson_objects", "glob"};
    if (!read_scans.count(name) && !StringUtil::StartsWith(name, "duckdb_") &&
        !StringUtil::StartsWith(name, "pragma_")) {
      throw InvalidInputException("POLICY_BLOCKED: sidequery.sql.query cannot verify read-only table function '%s'; use sidequery.sql.execute", name);
    }
  }

  static void CheckReadOnlyPlan(LogicalOperator &plan) {
    // Database modification properties do not describe external table-function
    // effects. Fail closed for scans whose read-only behavior is not known.
    if (plan.type == LogicalOperatorType::LOGICAL_GET) {
      auto &scan = plan.Cast<LogicalGet>();
#ifdef SQLNB_HAS_USER_EXTENSION_REPOSITORIES
      auto name = StringUtil::Lower(scan.function.GetName().GetIdentifierName());
#else
      auto name = StringUtil::Lower(scan.function.name);
#endif
      CheckReadOnlyScan(name);
    }
    LogicalOperatorVisitor::EnumerateExpressions(plan, [](unique_ptr<Expression> *expression) {
      if ((*expression)->IsVolatile()) {
        throw InvalidInputException("POLICY_BLOCKED: sidequery.sql.query rejects volatile functions; use sidequery.sql.execute");
      }
    });
    for (auto &child : plan.children) {
      CheckReadOnlyPlan(*child);
    }
  }

  string Execute(const char *sql, uint64_t max_rows, uint64_t max_bytes, bool read_only) {
    max_rows = std::min<uint64_t>(max_rows, 10000);
    max_bytes = std::min<uint64_t>(max_bytes, 4 * 1024 * 1024);
    auto statements = connection.ExtractStatements(sql);
    if (statements.size() != 1) {
      throw InvalidInputException("MCP SQL tools require exactly one SQL statement");
    }
#ifdef SQLNB_HAS_RESULT_STREAM
    auto statement_type = statements[0]->type;
#endif
    unique_ptr<PreparedStatement> prepared;
    unique_ptr<QueryResult> result;
    if (read_only) {
      if (statements[0]->type != StatementType::SELECT_STATEMENT) {
        throw InvalidInputException("POLICY_BLOCKED: sidequery.sql.query requires a read-only SELECT; use sidequery.sql.execute");
      }
      connection.context->RunFunctionInTransaction([&]() {
        CheckReadOnlyNode(*statements[0]->Cast<SelectStatement>().node);
      });
      prepared = connection.Prepare(std::move(statements[0]));
      if (prepared->HasError()) {
        throw InvalidInputException("%s", prepared->GetError());
      }
      if (!prepared->GetStatementProperties().modified_databases.empty()) {
        throw InvalidInputException("POLICY_BLOCKED: sidequery.sql.query rejects database mutations; use sidequery.sql.execute");
      }
      auto plan = connection.ExtractPlan(sql);
      CheckReadOnlyPlan(*plan);
#ifdef SQLNB_HAS_RESULT_STREAM
      result = prepared->Submit();
#else
      result = prepared->Execute();
#endif
    } else {
#ifdef SQLNB_HAS_RESULT_STREAM
      result = connection.Submit(std::move(statements[0]));
#else
      result = connection.SendQuery(std::move(statements[0]));
#endif
    }
    if (!result || result->HasError()) {
      throw IOException("%s", result ? result->GetError() : "query returned no result");
    }
#ifdef SQLNB_HAS_RESULT_STREAM
    // Final 2.0 separates submitted queries from their streaming readers.
    // Only read-only SELECTs may stop early; drain writes so truncating their
    // returned rows never rolls back an otherwise successful statement.
    if (StreamsByDefault(statement_type, *result, false)) {
      QueryResultStream<> stream(std::move(result));
      return SerializeResult(stream, max_rows, max_bytes);
    }
    result->Collection();
#endif
    return SerializeResult(*result, max_rows, max_bytes);
  }

  template <class RESULT>
  static string SerializeResult(RESULT &result, uint64_t max_rows, uint64_t max_bytes) {
    string response = "{\"columns\":[";
    string types = "],\"types\":[";
    vector<string> names;
    unordered_set<string> used_names;
    for (idx_t column = 0; column < result.ColumnCount(); column++) {
#ifdef SQLNB_HAS_USER_EXTENSION_REPOSITORIES
      auto original_name = result.ColumnName(column).GetIdentifierName();
      auto type = result.GetTypes()[column];
#else
      auto original_name = result.names[column];
      auto type = result.types[column];
#endif
      auto name = original_name;
      auto suffix = 1;
      while (!used_names.insert(name).second) {
        name = original_name + "_" + std::to_string(suffix++);
      }
      names.push_back(name);
      response += (column ? "," : "") + JSONEscape(name);
      types += (column ? "," : "") + JSONEscape(type.ToString());
    }
    response += types + "],\"rows\":[";
    if (response.size() + 128 > max_bytes) {
      throw InvalidInputException("query column metadata exceeds result byte limit");
    }
    uint64_t count = 0;
    bool truncated = false;
    while (auto chunk = result.Fetch()) {
      for (idx_t row = 0; row < chunk->size(); row++) {
        if (count >= max_rows) {
          truncated = true;
          break;
        }
        string object = "{";
        for (idx_t column = 0; column < names.size(); column++) {
          object += (column ? "," : "") + JSONEscape(names[column]) + ":" +
                    JSONValue(chunk->GetValue(column, row));
          if (response.size() + object.size() + 128 > max_bytes) {
            truncated = true;
            break;
          }
        }
        if (truncated) {
          break;
        }
        response += (count ? "," : "") + object + "}";
        count++;
      }
      if (truncated) {
        break;
      }
    }
    if (result.HasError()) {
      throw IOException("%s", result.GetError());
    }
    return response + "],\"returned_rows\":" + std::to_string(count) +
           ",\"truncated\":" + (truncated ? "true}" : "false}");
  }

  Connection connection;
  std::mutex query_lock;
  std::mutex active_lock;
  uint64_t active_id = 0;
  bool cancelled = false;
};

class SqlnbServer {
public:
  SqlnbServer(shared_ptr<DatabaseInstance> database_p, const string &remote_url,
              const string &local_dir, int requested_port, const string &notebook_root, bool mcp)
      : database(database_p), quack(StartQuack(*database_p)) {
    if (local_dir.empty() && remote_url.empty()) {
      StopQuack(*database_p, quack.listen_uri);
      throw InvalidInputException("remote asset URL or local asset directory is required");
    }
    auto quack_url = quack.listen_url;
    if (quack_url.empty() || quack_url.back() != '/') {
      quack_url.push_back('/');
    }
    quack_url += "quack";
    if (mcp) {
      mcp_token = RandomHex(32);
      mcp_connection = make_uniq<McpConnection>(*database_p);
    }
    auto config = "{\"port\":" + std::to_string(requested_port) +
                  ",\"localAssetDirectory\":" + JSONEscape(local_dir) +
                  ",\"remoteAssetUrl\":" + JSONEscape(remote_url) +
                  ",\"quackUrl\":" + JSONEscape(quack_url) +
                  ",\"token\":" + JSONEscape(quack.token) +
                  ",\"duckdbVersion\":" + JSONEscape(DuckDB::LibraryVersion()) +
                  ",\"notebookRoot\":" + JSONEscape(notebook_root) +
                  ",\"mcpToken\":" + JSONEscape(mcp_token) + "}";
    char *error = nullptr;
    http_server = sidequery_http_start_with_query(config.c_str(), mcp_connection.get(),
                                                  McpConnection::Query, McpConnection::Interrupt,
                                                  McpConnection::Free, &error);
    if (!http_server) {
      string message = error ? error : "unknown error";
      if (error) {
        sidequery_http_string_free(error);
      }
      StopQuack(*database_p, quack.listen_uri);
      throw IOException("failed to start SQLNB HTTP server: %s", message);
    }
    auto actual_port = sidequery_http_port(http_server);
    if (actual_port == 0) {
      sidequery_http_stop(http_server);
      http_server = nullptr;
      StopQuack(*database_p, quack.listen_uri);
      throw IOException("SQLNB HTTP server returned an invalid port");
    }
    origin = "http://127.0.0.1:" + std::to_string(actual_port);
  }

  ~SqlnbServer() {
    try {
      Stop();
    } catch (...) {
    }
  }

  string LaunchURL() const { return origin + "/#sidequery_token=" + quack.token; }
  string Origin() const { return origin; }
  string McpStatus() const {
    return mcp_token.empty() ? "{\"enabled\":false}"
                            : "{\"enabled\":true,\"url\":" + JSONEscape(origin + "/mcp") +
                                  ",\"token\":" + JSONEscape(mcp_token) + "}";
  }

  void Stop() {
    if (http_server) {
      sidequery_http_stop(http_server);
      http_server = nullptr;
    }
    mcp_connection.reset();
    if (!quack.listen_uri.empty()) {
      auto listen_uri = std::move(quack.listen_uri);
      auto owner = database.lock();
      if (owner) {
        StopQuack(*owner, listen_uri);
      }
    }
  }

private:
  weak_ptr<DatabaseInstance> database;
  QuackEndpoint quack;
  void *http_server = nullptr;
  string origin;
  string mcp_token;
  unique_ptr<McpConnection> mcp_connection;
};

class SqlnbState : public StorageExtensionInfo {
public:
  string Start(shared_ptr<DatabaseInstance> database, const string &remote_url, const string &local_dir,
               int port, bool open_browser, const string &notebook_root, bool mcp, connection_t caller) {
    std::lock_guard<std::mutex> guard(lock);
    if (server) {
      throw InvalidInputException("SQLNB is already running");
    }
    auto candidate = make_uniq<SqlnbServer>(std::move(database), remote_url, local_dir, port,
                                            notebook_root, mcp);
    auto launch_url = candidate->LaunchURL();
    if (open_browser) {
      OpenBrowser(launch_url);
    }
    server = std::move(candidate);
    owner_connection = caller;
    return StatusJSON(launch_url);
  }

  string Status(connection_t caller) {
    std::lock_guard<std::mutex> guard(lock);
    if (!server) {
      return "{\"running\":false,\"bridgeVersion\":1,\"protocol\":\"quack\"}";
    }
    RequireOwner(caller, "sqlnb_status");
    return StatusJSON(server->LaunchURL());
  }

  bool Stop(connection_t caller) {
    unique_ptr<SqlnbServer> stopped;
    {
      std::lock_guard<std::mutex> guard(lock);
      if (!server) {
        return false;
      }
      RequireOwner(caller, "sqlnb_stop");
      stopped = std::move(server);
      owner_connection = DConstants::INVALID_INDEX;
    }
    stopped->Stop();
    return true;
  }

private:
  void RequireOwner(connection_t caller, const char *function) const {
    if (caller != owner_connection) {
      throw InvalidInputException("%s may only be called from the connection that started SQLNB",
                                  function);
    }
  }

  string StatusJSON(const string &launch_url) const {
    return "{\"running\":true,\"bridgeVersion\":1,\"protocol\":\"quack\","
           "\"engine\":\"native-duckdb\",\"baseUrl\":" +
           JSONEscape(server->Origin()) + ",\"launchUrl\":" + JSONEscape(launch_url) +
           ",\"mcp\":" + server->McpStatus() + "}";
  }

  static void OpenBrowser(const string &url) {
#ifndef _WIN32
    const char *program;
#ifdef __APPLE__
    program = "open";
#else
    program = "xdg-open";
#endif
    pid_t pid;
    char *arguments[] = {const_cast<char *>(program), const_cast<char *>(url.c_str()), nullptr};
    if (posix_spawnp(&pid, program, nullptr, nullptr, arguments, environ) != 0) {
      throw IOException("failed to open SQLNB in the browser");
    }
    std::thread([pid] {
      int status;
      while (waitpid(pid, &status, 0) < 0 && errno == EINTR) {
      }
    }).detach();
#else
    if (reinterpret_cast<intptr_t>(
            ShellExecuteA(nullptr, "open", url.c_str(), nullptr, nullptr, SW_SHOWNORMAL)) <= 32) {
      throw IOException("failed to open SQLNB in the browser");
    }
#endif
  }

  std::mutex lock;
  unique_ptr<SqlnbServer> server;
  connection_t owner_connection = DConstants::INVALID_INDEX;
};

static constexpr const char *SQLNB_STATE_KEY = "sqlnb_internal";

static SqlnbState &GetState(DatabaseInstance &database) {
  auto extension = StorageExtension::Find(database.config, SQLNB_STATE_KEY);
  if (!extension || !extension->storage_info) {
    throw InternalException("SQLNB state is unavailable");
  }
  return *static_cast<SqlnbState *>(extension->storage_info.get());
}

class McpOwnerCleanup : public ClientContextState {
public:
  McpOwnerCleanup(SqlnbState &state_p, connection_t owner_p) : state(state_p), owner(owner_p) {}
  ~McpOwnerCleanup() override {
    try {
      state.Stop(owner);
    } catch (...) {
    }
  }
private:
  SqlnbState &state;
  connection_t owner;
};

static void StartFunction(DataChunk &args, ExpressionState &state, Vector &result) {
  if (!args.AllConstant()) {
    throw InvalidInputException("sqlnb_start arguments must be constant");
  }
  for (idx_t column = 0; column < args.ColumnCount(); column++) {
    if (args.data[column].GetValue(0).IsNull()) {
      throw InvalidInputException("sqlnb_start arguments must not be NULL");
    }
  }
  auto remote_url = args.ColumnCount() > 0 ? args.data[0].GetValue(0).GetValue<string>()
                                           : string(DEFAULT_SQLNB_UI);
  auto local_dir = args.ColumnCount() > 1 ? args.data[1].GetValue(0).GetValue<string>() : string();
  auto port = args.ColumnCount() > 2 ? args.data[2].GetValue(0).GetValue<int32_t>() : 0;
  auto open_browser = args.ColumnCount() > 3 ? args.data[3].GetValue(0).GetValue<bool>() : true;
  auto notebook_root = args.ColumnCount() > 4 ? args.data[4].GetValue(0).GetValue<string>() : string();
  auto mcp = args.ColumnCount() > 5 ? args.data[5].GetValue(0).GetValue<bool>() : false;
  if (port < 0 || port > 65535) {
    throw InvalidInputException("port must be between 0 and 65535");
  }
  if (!Settings::Get<EnableExternalAccessSetting>(state.GetContext())) {
    throw PermissionException("SQLNB requires enable_external_access=true for notebook file access");
  }
  auto &database = *state.GetContext().db;
  result.SetValue(0, Value(GetState(database).Start(state.GetContext().db, remote_url, local_dir, port,
                                                    open_browser, notebook_root, mcp,
                                                    state.GetContext().GetConnectionId())));
  if (mcp) {
    state.GetContext().registered_state->GetOrCreate<McpOwnerCleanup>(
        "sqlnb_mcp_owner", GetState(database), state.GetContext().GetConnectionId());
  }
  result.SetVectorType(VectorType::CONSTANT_VECTOR);
}

static void StatusFunction(DataChunk &, ExpressionState &state, Vector &result) {
  result.SetValue(0, Value(GetState(*state.GetContext().db).Status(state.GetContext().GetConnectionId())));
  result.SetVectorType(VectorType::CONSTANT_VECTOR);
}

static void StopFunction(DataChunk &, ExpressionState &state, Vector &result) {
  result.SetValue(0,
                  Value::BOOLEAN(GetState(*state.GetContext().db).Stop(state.GetContext().GetConnectionId())));
  result.SetVectorType(VectorType::CONSTANT_VECTOR);
}

static void LoadInternal(ExtensionLoader &loader) {
  auto state_extension = make_shared_ptr<StorageExtension>();
  state_extension->storage_info = make_uniq<SqlnbState>();
  StorageExtension::Register(loader.GetDatabaseInstance().config, SQLNB_STATE_KEY,
                             state_extension);

#ifdef SQLNB_HAS_USER_EXTENSION_REPOSITORIES
  FunctionSignature sqlnb_start_signature(
      {FunctionParameter("ui", LogicalType::VARCHAR, Value(DEFAULT_SQLNB_UI)),
       FunctionParameter("assets", LogicalType::VARCHAR, Value("")),
       FunctionParameter("port", LogicalType::INTEGER, Value::INTEGER(0)),
       FunctionParameter("open_browser", LogicalType::BOOLEAN, Value::BOOLEAN(true)),
       FunctionParameter("notebook_root", LogicalType::VARCHAR, Value("")),
       FunctionParameter("mcp", LogicalType::BOOLEAN, Value::BOOLEAN(false))},
      LogicalType::VARCHAR);
  ScalarFunction sqlnb_start("sqlnb_start", std::move(sqlnb_start_signature), StartFunction);
  sqlnb_start.SetVolatile();
  sqlnb_start.SetFallible();
  loader.RegisterFunction(sqlnb_start);
#else
  ScalarFunction sqlnb_start("sqlnb_start", {}, LogicalType::VARCHAR, StartFunction);
  sqlnb_start.SetVolatile();
  sqlnb_start.SetFallible();
  loader.RegisterFunction(sqlnb_start);
#endif

  ScalarFunctionSet compatibility_starts("sidequery_ui_start");
  ScalarFunction start_four({LogicalType::VARCHAR, LogicalType::VARCHAR, LogicalType::INTEGER,
                             LogicalType::BOOLEAN},
                            LogicalType::VARCHAR, StartFunction);
  start_four.SetVolatile();
  start_four.SetFallible();
  compatibility_starts.AddFunction(start_four);
  ScalarFunction start_five({LogicalType::VARCHAR, LogicalType::VARCHAR, LogicalType::INTEGER,
                             LogicalType::BOOLEAN, LogicalType::VARCHAR},
                            LogicalType::VARCHAR, StartFunction);
  start_five.SetVolatile();
  start_five.SetFallible();
  compatibility_starts.AddFunction(start_five);
  ScalarFunction start_six({LogicalType::VARCHAR, LogicalType::VARCHAR, LogicalType::INTEGER,
                            LogicalType::BOOLEAN, LogicalType::VARCHAR, LogicalType::BOOLEAN},
                           LogicalType::VARCHAR, StartFunction);
  start_six.SetVolatile();
  start_six.SetFallible();
  compatibility_starts.AddFunction(start_six);
  loader.RegisterFunction(compatibility_starts);

  ScalarFunction status("sqlnb_status", {}, LogicalType::VARCHAR, StatusFunction);
  status.SetVolatile();
  status.SetFallible();
  loader.RegisterFunction(status);
  ScalarFunction compatibility_status("sidequery_ui_status", {}, LogicalType::VARCHAR, StatusFunction);
  compatibility_status.SetVolatile();
  compatibility_status.SetFallible();
  loader.RegisterFunction(compatibility_status);

  ScalarFunction stop("sqlnb_stop", {}, LogicalType::BOOLEAN, StopFunction);
  stop.SetVolatile();
  stop.SetFallible();
  loader.RegisterFunction(stop);
  ScalarFunction compatibility_stop("sidequery_ui_stop", {}, LogicalType::BOOLEAN, StopFunction);
  compatibility_stop.SetVolatile();
  compatibility_stop.SetFallible();
  loader.RegisterFunction(compatibility_stop);
}

} // namespace

void SqlnbExtension::Load(ExtensionLoader &loader) { LoadInternal(loader); }
std::string SqlnbExtension::Name() { return "sqlnb"; }
std::string SqlnbExtension::Version() const { return "v0.1.0"; }

} // namespace duckdb

extern "C" {
DUCKDB_CPP_EXTENSION_ENTRY(sqlnb, loader) {
  duckdb::LoadInternal(loader);
}
}
