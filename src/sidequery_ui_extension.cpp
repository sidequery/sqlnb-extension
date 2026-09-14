// Include the platform declarations before DuckDB defines its restrict macro.
#ifndef _WIN32
#include <spawn.h>
#include <sys/wait.h>
#endif

#include "sidequery_ui_extension.hpp"
#include "sidequery_http.h"

#include "duckdb/main/connection.hpp"
#include "duckdb/main/database.hpp"
#include "duckdb/main/extension/extension_loader.hpp"
#include "duckdb/main/extension_helper.hpp"
#include "duckdb/main/settings.hpp"
#include "duckdb/storage/storage_extension.hpp"

#if __has_include("duckdb/main/extension_repository_manager.hpp")
#include "duckdb/main/extension_repository_manager.hpp"
#define SQLNB_HAS_USER_EXTENSION_REPOSITORIES 1
#endif

#include <openssl/rand.h>

#include <cerrno>
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

class SqlnbServer {
public:
  SqlnbServer(shared_ptr<DatabaseInstance> database_p, const string &remote_url,
              const string &local_dir, int requested_port, const string &notebook_root)
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
    auto config = "{\"port\":" + std::to_string(requested_port) +
                  ",\"localAssetDirectory\":" + JSONEscape(local_dir) +
                  ",\"remoteAssetUrl\":" + JSONEscape(remote_url) +
                  ",\"quackUrl\":" + JSONEscape(quack_url) +
                  ",\"token\":" + JSONEscape(quack.token) +
                  ",\"duckdbVersion\":" + JSONEscape(DuckDB::LibraryVersion()) +
                  ",\"notebookRoot\":" + JSONEscape(notebook_root) + "}";
    char *error = nullptr;
    http_server = sidequery_http_start(config.c_str(), &error);
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

  void Stop() {
    if (http_server) {
      sidequery_http_stop(http_server);
      http_server = nullptr;
    }
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
};

class SqlnbState : public StorageExtensionInfo {
public:
  string Start(shared_ptr<DatabaseInstance> database, const string &remote_url, const string &local_dir,
               int port, bool open_browser, const string &notebook_root, connection_t caller) {
    std::lock_guard<std::mutex> guard(lock);
    if (server) {
      throw InvalidInputException("SQLNB is already running");
    }
    auto candidate = make_uniq<SqlnbServer>(std::move(database), remote_url, local_dir, port,
                                            notebook_root);
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
           JSONEscape(server->Origin()) + ",\"launchUrl\":" + JSONEscape(launch_url) + "}";
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
  if (port < 0 || port > 65535) {
    throw InvalidInputException("port must be between 0 and 65535");
  }
  if (!Settings::Get<EnableExternalAccessSetting>(state.GetContext())) {
    throw PermissionException("SQLNB requires enable_external_access=true for notebook file access");
  }
  auto &database = *state.GetContext().db;
  result.SetValue(0, Value(GetState(database).Start(state.GetContext().db, remote_url, local_dir, port,
                                                    open_browser, notebook_root,
                                                    state.GetContext().GetConnectionId())));
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
       FunctionParameter("notebook_root", LogicalType::VARCHAR, Value(""))},
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
