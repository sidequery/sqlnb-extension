#pragma once

// Included inside sqlnb's implementation namespace, after JSONEscape.
struct DuckDBUINotebooks {
  unique_ptr<DuckDB> external_database;
  unique_ptr<Connection> connection;
  string catalog;

  DuckDBUINotebooks(ClientContext &context, bool writable) {
    if (!Settings::Get<EnableExternalAccessSetting>(context)) {
      throw PermissionException("DuckDB UI notebooks require enable_external_access=true");
    }
    auto &fs = FileSystem::GetFileSystem(context);
    auto path = fs.ExpandPath("~/.duckdb/extension_data/ui/ui.db");
    connection = make_uniq<Connection>(*context.db);
    auto databases = Checked(connection->Query(
        "select database_name from duckdb_databases() where type = 'duckdb' "
        "and (database_name = '_duckdb_ui' or path = ?) order by database_name = '_duckdb_ui' desc", Value(path)));
    if (auto chunk = databases->Fetch()) {
      catalog = chunk->GetValue(0, 0).GetValue<string>();
      return;
    }
    if (!fs.FileExists(path)) {
      connection.reset();
      return; // Browsing must never create or migrate the official database.
    }
    DBConfig config;
    config.options.access_mode = writable ? AccessMode::READ_WRITE : AccessMode::READ_ONLY;
    external_database = make_uniq<DuckDB>(path, &config);
    connection = make_uniq<Connection>(*external_database);
    auto current = Checked(connection->Query("select current_database()"))->Fetch();
    catalog = current->GetValue(0, 0).GetValue<string>();
  }

  static unique_ptr<QueryResult> Checked(unique_ptr<QueryResult> result) {
    if (result->HasError()) throw IOException("DuckDB UI notebooks: %s", result->GetError());
    return result;
  }

  string Table(const string &name) const {
    return "\"" + StringUtil::Replace(catalog, "\"", "\"\"") + "\".main." + name;
  }

  string Read(const string &id) {
    if (!connection) throw InvalidInputException("DuckDB UI notebook database does not exist");
    auto result = Checked(connection->Query(
        "select n.id::varchar, v.version, v.title, v.json from " + Table("notebooks") +
        " n join " + Table("notebook_versions") + " v on n.id=v.notebook_id "
        "where n.id=?::uuid and v.expires is null", Value::UUID(id)));
    auto chunk = result->Fetch();
    if (!chunk || chunk->size() != 1 || result->Fetch()) {
      throw InvalidInputException("DuckDB UI notebook is missing or has multiple current versions");
    }
    return Record(*chunk, 0);
  }

  static string Record(DataChunk &chunk, idx_t row) {
    return "{\"id\":" + JSONEscape(chunk.GetValue(0, row).GetValue<string>()) +
           ",\"version\":" + chunk.GetValue(1, row).ToString() +
           ",\"title\":" + JSONEscape(chunk.GetValue(2, row).GetValue<string>()) +
           ",\"json\":" + JSONEscape(chunk.GetValue(3, row).GetValue<string>()) + "}";
  }
};

static void DuckDBUIList(DataChunk &, ExpressionState &state, Vector &result) {
  DuckDBUINotebooks store(state.GetContext(), false);
  string json = "[";
  if (store.connection) {
    auto records = DuckDBUINotebooks::Checked(store.connection->Query(
        "select n.id::varchar, v.version, v.title from " + store.Table("notebooks") +
        " n join " + store.Table("notebook_versions") + " v on n.id=v.notebook_id "
        "where v.expires is null order by v.title, n.id"));
    bool first = true;
    while (auto chunk = records->Fetch()) {
      for (idx_t row = 0; row < chunk->size(); row++) {
        if (!first) json += ",";
        first = false;
        json += "{\"id\":" + JSONEscape(chunk->GetValue(0, row).GetValue<string>()) +
                ",\"version\":" + chunk->GetValue(1, row).ToString() +
                ",\"title\":" + JSONEscape(chunk->GetValue(2, row).GetValue<string>()) + "}";
      }
    }
  }
  result.SetValue(0, Value(json + "]"));
  result.SetVectorType(VectorType::CONSTANT_VECTOR);
}

static void RequireDuckDBUIConstants(DataChunk &args) {
  if (!args.AllConstant()) throw InvalidInputException("DuckDB UI notebook arguments must be constant");
  for (idx_t i = 0; i < args.ColumnCount(); i++) {
    if (args.data[i].GetValue(0).IsNull()) throw InvalidInputException("DuckDB UI notebook arguments must not be NULL");
  }
}

static void DuckDBUIRead(DataChunk &args, ExpressionState &state, Vector &result) {
  RequireDuckDBUIConstants(args);
  DuckDBUINotebooks store(state.GetContext(), false);
  result.SetValue(0, Value(store.Read(args.data[0].GetValue(0).GetValue<string>())));
  result.SetVectorType(VectorType::CONSTANT_VECTOR);
}

static void DuckDBUISave(DataChunk &args, ExpressionState &state, Vector &result) {
  RequireDuckDBUIConstants(args);
  auto id = args.data[0].GetValue(0).GetValue<string>();
  auto expected = args.data[1].GetValue(0).GetValue<int32_t>();
  auto title = args.data[2].GetValue(0).GetValue<string>();
  auto edited = args.data[3].GetValue(0).GetValue<string>();
  if (expected < 1 || expected == NumericLimits<int32_t>::Maximum()) {
    throw InvalidInputException("Invalid DuckDB UI notebook version");
  }
  DuckDBUINotebooks store(state.GetContext(), true);
  if (!store.connection) throw InvalidInputException("DuckDB UI notebook database does not exist");
  auto &connection = *store.connection;
  DuckDBUINotebooks::Checked(connection.Query("begin transaction"));
  try {
    auto current = DuckDBUINotebooks::Checked(connection.Query(
        "select v.version, v.json from " + store.Table("notebooks") + " n join " +
        store.Table("notebook_versions") + " v on n.id=v.notebook_id "
        "where n.id=?::uuid and v.expires is null", Value::UUID(id)));
    auto chunk = current->Fetch();
    if (!chunk || chunk->size() != 1 || current->Fetch() ||
        chunk->GetValue(0, 0).GetValue<int32_t>() != expected) {
      throw InvalidInputException("REVISION_CONFLICT: DuckDB UI notebook changed; reopen it before saving. Your editor changes are preserved.");
    }
    auto previous = chunk->GetValue(1, 0).GetValue<string>();
    char *error = nullptr;
    auto encoded = sidequery_duckdb_ui_next_version(previous.c_str(), edited.c_str(), &error);
    if (!encoded) {
      string message = error ? error : "Invalid DuckDB UI notebook";
      if (error) sidequery_http_string_free(error);
      throw InvalidInputException("%s", message);
    }
    string json(encoded);
    sidequery_http_string_free(encoded);
    DuckDBUINotebooks::Checked(connection.Query(
        "insert into " + store.Table("notebook_versions") +
        " (notebook_id,version,title,json,created,expires) values (?::uuid,?,?,?,timezone('UTC',current_timestamp),NULL)",
        Value::UUID(id), expected + 1, Value(title), Value(json)));
    DuckDBUINotebooks::Checked(connection.Query(
        "update " + store.Table("notebook_versions") +
        " set expires=timezone('UTC',current_timestamp)+interval 7 days where notebook_id=?::uuid and version=? and expires is null",
        Value::UUID(id), expected));
    auto saved = store.Read(id);
    DuckDBUINotebooks::Checked(connection.Query("commit"));
    result.SetValue(0, Value(saved));
    result.SetVectorType(VectorType::CONSTANT_VECTOR);
  } catch (...) {
    connection.Query("rollback");
    throw;
  }
}

// Caller owns the transaction. New identities start fresh version sequences;
// the validator preserves every unknown notebook and cell metadata field.
static string InsertDuckDBUINotebook(DuckDBUINotebooks &store, const string &title, const string &source) {
  char *error = nullptr;
  auto encoded = sidequery_duckdb_ui_next_version(nullptr, source.c_str(), &error);
  if (!encoded) {
    string message = error ? error : "Invalid DuckDB UI notebook";
    if (error) sidequery_http_string_free(error);
    throw InvalidInputException("%s", message);
  }
  string json(encoded);
  sidequery_http_string_free(encoded);
  auto inserted = DuckDBUINotebooks::Checked(store.connection->Query(
      "insert into " + store.Table("notebooks") +
      " (id,name,created) values (uuid(),?,timezone('UTC',current_timestamp)) returning id::varchar", Value(title)));
  auto chunk = inserted->Fetch();
  auto id = chunk->GetValue(0, 0).GetValue<string>();
  DuckDBUINotebooks::Checked(store.connection->Query(
      "insert into " + store.Table("notebook_versions") +
      " (notebook_id,version,title,json,created,expires) values (?::uuid,1,?,?,timezone('UTC',current_timestamp),NULL)",
      Value::UUID(id), Value(title), Value(json)));
  return store.Read(id);
}

static string CheckDuckDBUIRevision(DuckDBUINotebooks &store, const string &id, int32_t expected) {
  auto current = DuckDBUINotebooks::Checked(store.connection->Query(
      "select v.version,v.json from " + store.Table("notebooks") + " n join " +
      store.Table("notebook_versions") + " v on n.id=v.notebook_id where n.id=?::uuid and v.expires is null",
      Value::UUID(id)));
  auto chunk = current->Fetch();
  if (expected < 1 || !chunk || chunk->size() != 1 || current->Fetch() ||
      chunk->GetValue(0, 0).GetValue<int32_t>() != expected) {
    throw InvalidInputException("REVISION_CONFLICT: DuckDB UI notebook changed; reopen it before continuing.");
  }
  // Establish a write conflict with another transaction changing this version.
  DuckDBUINotebooks::Checked(store.connection->Query(
      "update " + store.Table("notebook_versions") +
      " set json=json where notebook_id=?::uuid and version=? and expires is null", Value::UUID(id), expected));
  return chunk->GetValue(1, 0).GetValue<string>();
}

static void DuckDBUICreate(DataChunk &args, ExpressionState &state, Vector &result) {
  RequireDuckDBUIConstants(args);
  DuckDBUINotebooks store(state.GetContext(), true);
  if (!store.connection) throw InvalidInputException("DuckDB UI notebook database does not exist");
  DuckDBUINotebooks::Checked(store.connection->Query("begin transaction"));
  try {
    auto record = InsertDuckDBUINotebook(store, args.data[0].GetValue(0).GetValue<string>(),
                                       args.data[1].GetValue(0).GetValue<string>());
    DuckDBUINotebooks::Checked(store.connection->Query("commit"));
    result.SetValue(0, Value(record));
    result.SetVectorType(VectorType::CONSTANT_VECTOR);
  } catch (...) {
    store.connection->Query("rollback");
    throw;
  }
}

static void DuckDBUIDuplicate(DataChunk &args, ExpressionState &state, Vector &result) {
  RequireDuckDBUIConstants(args);
  DuckDBUINotebooks store(state.GetContext(), true);
  if (!store.connection) throw InvalidInputException("DuckDB UI notebook database does not exist");
  DuckDBUINotebooks::Checked(store.connection->Query("begin transaction"));
  try {
    auto source = CheckDuckDBUIRevision(store, args.data[0].GetValue(0).GetValue<string>(),
                                       args.data[1].GetValue(0).GetValue<int32_t>());
    auto record = InsertDuckDBUINotebook(store, args.data[2].GetValue(0).GetValue<string>(), source);
    DuckDBUINotebooks::Checked(store.connection->Query("commit"));
    result.SetValue(0, Value(record));
    result.SetVectorType(VectorType::CONSTANT_VECTOR);
  } catch (...) {
    store.connection->Query("rollback");
    throw;
  }
}

static void DuckDBUIDelete(DataChunk &args, ExpressionState &state, Vector &result) {
  RequireDuckDBUIConstants(args);
  auto id = args.data[0].GetValue(0).GetValue<string>();
  DuckDBUINotebooks store(state.GetContext(), true);
  if (!store.connection) throw InvalidInputException("DuckDB UI notebook database does not exist");
  DuckDBUINotebooks::Checked(store.connection->Query("begin transaction"));
  try {
    CheckDuckDBUIRevision(store, id, args.data[1].GetValue(0).GetValue<int32_t>());
    // Delete only this notebook's records and its selected-ID preference.
    // Never execute notebook SQL or touch the databases referenced by cells.
    DuckDBUINotebooks::Checked(store.connection->Query(
        "delete from " + store.Table("notebook_versions") + " where notebook_id=?::uuid", Value::UUID(id)));
    DuckDBUINotebooks::Checked(store.connection->Query(
        "delete from " + store.Table("notebooks") + " where id=?::uuid", Value::UUID(id)));
    DuckDBUINotebooks::Checked(store.connection->Query(
        "delete from " + store.Table("current_notebook_id") + " where id=?::uuid", Value::UUID(id)));
    DuckDBUINotebooks::Checked(store.connection->Query("commit"));
    result.SetValue(0, Value("{\"id\":" + JSONEscape(id) + "}"));
    result.SetVectorType(VectorType::CONSTANT_VECTOR);
  } catch (...) {
    store.connection->Query("rollback");
    throw;
  }
}

static void RegisterDuckDBUINotebooks(ExtensionLoader &loader) {
  ScalarFunction list("sqlnb_duckdb_ui_notebooks", {}, LogicalType::VARCHAR, DuckDBUIList);
  list.SetVolatile();
  list.SetFallible();
  loader.RegisterFunction(list);
  ScalarFunction read("sqlnb_duckdb_ui_notebook", {LogicalType::VARCHAR}, LogicalType::VARCHAR, DuckDBUIRead);
  read.SetVolatile();
  read.SetFallible();
  loader.RegisterFunction(read);
  ScalarFunction save("sqlnb_duckdb_ui_save", {LogicalType::VARCHAR, LogicalType::INTEGER, LogicalType::VARCHAR, LogicalType::VARCHAR},
                      LogicalType::VARCHAR, DuckDBUISave);
  save.SetVolatile();
  save.SetFallible();
  loader.RegisterFunction(save);
  ScalarFunction create("sqlnb_duckdb_ui_create", {LogicalType::VARCHAR, LogicalType::VARCHAR}, LogicalType::VARCHAR, DuckDBUICreate);
  create.SetVolatile();
  create.SetFallible();
  loader.RegisterFunction(create);
  ScalarFunction duplicate("sqlnb_duckdb_ui_duplicate", {LogicalType::VARCHAR, LogicalType::INTEGER, LogicalType::VARCHAR}, LogicalType::VARCHAR, DuckDBUIDuplicate);
  duplicate.SetVolatile();
  duplicate.SetFallible();
  loader.RegisterFunction(duplicate);
  ScalarFunction remove("sqlnb_duckdb_ui_delete", {LogicalType::VARCHAR, LogicalType::INTEGER}, LogicalType::VARCHAR, DuckDBUIDelete);
  remove.SetVolatile();
  remove.SetFallible();
  loader.RegisterFunction(remove);
}
