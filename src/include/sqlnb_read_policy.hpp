#pragma once

// Instant SQL read policy.
//
// This policy deliberately accepts a subset of SQL. It parses without extension
// hooks and never PREPAREs, EXPLAINs or executes unverified SQL to classify it.
// DuckDB binding itself can run code (AT clauses, type parameters, table
// function binds, replacement scans, macro expansion), so the parsed statement
// is verified before binding, in the binding transaction, resolving every name
// through a CatalogEntryRetriever configured exactly as DuckDB's binder
// configures its own (including the search path DuckDB uses inside views).
// Anything not positively recognised is refused with a reason; refused
// statements remain available through explicit Run, never Instant SQL.
#if __has_include("duckdb/main/query_result_stream.hpp")
#include "duckdb/catalog/catalog.hpp"
#include "duckdb/catalog/catalog_entry/table_catalog_entry.hpp"
#include "duckdb/catalog/catalog_entry/type_catalog_entry.hpp"
#include "duckdb/catalog/catalog_entry/view_catalog_entry.hpp"
#include "duckdb/catalog/catalog_entry_retriever.hpp"
#include "duckdb/catalog/catalog_search_path.hpp"
#include "duckdb/catalog/entry_lookup_info.hpp"
#include "duckdb/common/enums/statement_type.hpp"
#include "duckdb/function/scalar_function.hpp"
#include "duckdb/function/table_function.hpp"
#include "duckdb/main/database_manager.hpp"
#include "duckdb/main/extension/extension_loader.hpp"
#include "duckdb/parser/parser.hpp"
#include "duckdb/parser/column_list.hpp"
#include "duckdb/parser/expression/cast_expression.hpp"
#include "duckdb/parser/expression/columnref_expression.hpp"
#include "duckdb/parser/expression/constant_expression.hpp"
#include "duckdb/parser/common_table_expression_info.hpp"
#include "duckdb/parser/parsed_expression_iterator.hpp"
#include "duckdb/parser/expression/function_expression.hpp"
#include "duckdb/parser/expression/subquery_expression.hpp"
#include "duckdb/parser/expression/type_expression.hpp"
#include "duckdb/parser/expression/window_expression.hpp"
#include "duckdb/parser/query_node/select_node.hpp"
#include "duckdb/parser/query_node/set_operation_node.hpp"
#include "duckdb/parser/statement/select_statement.hpp"
#include "duckdb/parser/tableref/basetableref.hpp"
#include "duckdb/parser/tableref/expressionlistref.hpp"
#include "duckdb/parser/tableref/joinref.hpp"
#include "duckdb/parser/tableref/subqueryref.hpp"
#include "duckdb/parser/tableref/table_function_ref.hpp"
#include "duckdb/planner/binder.hpp"
#include "duckdb/planner/binding_alias.hpp"
#include "duckdb/planner/planner_extension.hpp"

namespace duckdb {
namespace sqlnb_read_policy {

//! Views may read other views. Past this nesting depth a statement needs an explicit Run.
static constexpr idx_t MAX_VIEW_DEPTH = 8;
//! Bounds the total number of view bodies verified for one statement, so views
//! that share (diamond) references cannot make verification exponential.
static constexpr idx_t MAX_VIEW_EXPANSIONS = 64;

class Policy {
public:
  //! `outer_binder` is the binder that will bind the returned statement. Its
  //! catalog search path and the CTEs of the query around it apply to the
  //! statement, so verification uses them too.
  explicit Policy(ClientContext &context_p, optional_ptr<Binder> outer_binder_p = nullptr)
      : context(context_p), outer_binder(outer_binder_p) {
    auto retriever = make_uniq<CatalogEntryRetriever>(context);
    if (outer_binder) retriever->Inherit(outer_binder->EntryRetriever());
    retrievers.push_back(std::move(retriever));
  }

  unique_ptr<SelectStatement> Parse(const string &sql) {
    auto parser = Parser::GetBuiltinParser();
    parser.ParseQuery(sql);
    if (parser.statements.empty()) Refuse("an empty statement");
    if (parser.statements.size() != 1) Refuse("more than one statement at a time");
    auto type = parser.statements[0]->type;
    if (type == StatementType::MULTI_STATEMENT) {
      // PIVOT expands into DDL that creates types before the query runs.
      Refuse("statements that DuckDB expands into several statements (such as PIVOT)");
    }
    if (type != StatementType::SELECT_STATEMENT) {
      // DML, DDL, PRAGMA, ATTACH, SET, COPY, CALL, EXPORT and every other kind.
      Refuse(StatementTypeToString(type) + " statements");
    }
    // A planner extension can turn any unresolved column name into arbitrary
    // SQL during binding; none ships with DuckDB itself.
    for (auto &extension : PlannerExtension::Iterate(context)) {
      if (extension.get_sql_value_function) Refuse("statements while an extension rewrites column names during binding");
    }
    auto statement = unique_ptr_cast<SQLStatement, SelectStatement>(std::move(parser.statements[0]));
    CheckNode(*statement->node);
    return statement;
  }

private:
  [[noreturn]] void Refuse(const string &reason) const {
    string message = "SQLNB_READ_ONLY: Instant SQL does not preview " + reason;
    if (!view_path.empty()) message += " (inside view " + StringUtil::Join(view_path, " > ") + ")";
    // Single-argument constructor: the message is never used as a format string.
    throw InvalidInputException(message + ". Use Run to execute this statement explicitly.");
  }

  static string Quote(const string &name) {
    return "'" + name + "'";
  }

  CatalogEntryRetriever &Retriever() {
    return *retrievers.back();
  }

  //! Resolves a function name exactly as the expression binder does and
  //! requires a DuckDB built-in (never a macro, UDF or third-party extension).
  void RequireBuiltin(const string &name, CatalogType lookup_type, const string &display, bool allow_window = false) {
    optional_ptr<CatalogEntry> entry;
    try {
      EntryLookupInfo lookup(lookup_type, QualifiedName(Identifier(name)));
      auto bound = Binder::BindTableName(Retriever(), QualifiedName(Identifier(name)));
      entry = Retriever().GetEntry(EntryLookupInfo(lookup, bound), OnEntryNotFound::RETURN_NULL);
    } catch (const Exception &) {
      Refuse(display + ", which could not be resolved");
    }
    if (!entry) Refuse(display + ", which does not exist");
    if (entry->type == CatalogType::MACRO_ENTRY || entry->type == CatalogType::TABLE_MACRO_ENTRY) {
      Refuse("the macro " + Quote(name) + " (macro bodies are not verified)");
    }
    if (!entry->internal) Refuse("the user-defined function " + Quote(name));
    auto extension = entry->extension_name.GetIdentifierName();
    // SQLNB loads DuckDB's ICU extension at startup; it replaces the date/time
    // value functions (current_date, ...) with time-zone-aware built-ins.
    if (!extension.empty() && extension != "core_functions" && extension != "icu") {
      Refuse("the function " + Quote(name) + " from the " + Quote(extension) + " extension");
    }
    bool accepted = false;
    switch (entry->type) {
    case CatalogType::SCALAR_FUNCTION_ENTRY:
      accepted = lookup_type == CatalogType::SCALAR_FUNCTION_ENTRY;
      break;
    case CatalogType::AGGREGATE_FUNCTION_ENTRY:
      accepted = lookup_type == CatalogType::SCALAR_FUNCTION_ENTRY;
      break;
    case CatalogType::WINDOW_FUNCTION_ENTRY:
      accepted = allow_window;
      break;
    case CatalogType::TABLE_FUNCTION_ENTRY:
      accepted = lookup_type == CatalogType::TABLE_FUNCTION_ENTRY;
      break;
    default:
      break;
    }
    if (!accepted) Refuse(display + ", which is not a built-in function of the expected kind");
  }

  static const unordered_set<string> &ScalarAllowlist() {
    static const unordered_set<string> scalars = {
      "+", "-", "*", "/", "//", "%", "**", "||", "abs", "ceil", "ceiling", "floor", "round", "trunc",
      "sqrt", "pow", "power", "exp", "ln", "log", "log10", "sign", "greatest", "least",
      "lower", "upper", "length", "char_length", "octet_length", "trim", "ltrim", "rtrim", "substring",
      "substr", "left", "right", "replace", "repeat", "concat", "concat_ws", "starts_with", "ends_with", "contains",
      "like_escape", "not_like_escape", "ilike_escape", "not_ilike_escape", "regexp_matches", "regexp_full_match",
      // Operators parse to these function names and bind like any function, so
      // a macro of the same name on the search path would replace them:
      // LIKE ~~, NOT LIKE !~~, ILIKE ~~*, NOT ILIKE !~~*, GLOB ~~~, ^@ starts_with,
      // bitwise & | << >> ~ xor, ^ power, and list containment @> <@ &&.
      // SIMILAR TO and ~ (regex) parse to regexp_full_match and regexp_matches.
      "~~", "!~~", "~~*", "!~~*", "~~~", "^@", "&", "|", "<<", ">>", "~", "xor", "^", "@>", "<@", "&&",
      "count", "count_star", "sum", "avg", "min", "max", "median", "stddev", "stddev_samp", "stddev_pop",
      "variance", "var_samp", "var_pop", "bool_and", "bool_or", "first", "last", "any_value", "string_agg",
      "list", "array_agg", "list_value", "struct_pack", "struct_extract", "array_extract", "list_extract",
      "array_slice", "list_slice", "array_length", "list_count", "list_sum", "list_min", "list_max",
      "date_part", "date_trunc", "date_diff", "datediff", "strftime", "strptime", "try_strptime"
    };
    return scalars;
  }

  void CheckFunction(FunctionExpression &function, bool table_function) {
    const auto name = StringUtil::Lower(function.FunctionName().GetIdentifierName());
    // A qualified call is either a catalog function or DuckDB's method-call
    // rewrite ("x.lower()"); the binder chooses between them by lookup.
    if (function.GetQualifiedName().Path().size() != 1) {
      Refuse("the qualified or method-style call " + Quote(function.GetQualifiedName().ToString()));
    }
    // Names are a first filter, checked before any catalog lookup: DuckDB
    // autoloads (and may install) extensions for unknown function names.
    static const unordered_set<string> scans = {"range", "generate_series", "unnest", "repeat"};
    if (table_function && !scans.count(name)) {
      Refuse("the table function " + Quote(name) + " (only range, generate_series, unnest and repeat are previewed; "
             "file, network and catalog scans need Run)");
    }
    if (!table_function && !ScalarAllowlist().count(name)) {
      Refuse("the function " + Quote(name) + " (only approved side-effect-free built-ins are previewed)");
    }
    // Resolve the actual entry to exclude user macros/UDFs shadowing an allowed
    // built-in and extension-provided overloads.
    RequireBuiltin(name, table_function ? CatalogType::TABLE_FUNCTION_ENTRY : CatalogType::SCALAR_FUNCTION_ENTRY,
                   (table_function ? "the table function " : "the function ") + Quote(name));
    ParsedExpressionIterator::EnumerateChildren(function, [this](ParsedExpression &child) { CheckExpression(child); });
  }

  void CheckWindow(WindowExpression &window) {
    static const unordered_set<string> window_functions = {
      "row_number", "rank", "dense_rank", "percent_rank", "cume_dist", "ntile",
      "lag", "lead", "first_value", "last_value", "nth_value"
    };
    const auto name = StringUtil::Lower(window.FunctionName().GetIdentifierName());
    if (window.GetQualifiedName().Path().size() != 1) {
      Refuse("the qualified window function " + Quote(window.GetQualifiedName().ToString()));
    }
    if (!window_functions.count(name) && !ScalarAllowlist().count(name)) {
      Refuse("the window function " + Quote(name));
    }
    // The binder resolves window names in the function namespace and expands a
    // macro of that name; require a built-in aggregate or window function.
    RequireBuiltin(name, CatalogType::SCALAR_FUNCTION_ENTRY, "the window function " + Quote(name), true);
    // Arguments, partitions, orders, frame offsets and FILTER are all children.
    // RANGE offsets bind +/- from the system catalog, never the search path.
    ParsedExpressionIterator::EnumerateChildren(window, [this](ParsedExpression &child) { CheckExpression(child); });
  }

  //! A column reference may bind to something other than a column: an implicit
  //! struct_pack of a whole relation, a struct/map field extraction, or a SQL
  //! value function. Each is a function the binder resolves through the search
  //! path, so each must resolve to a built-in.
  void CheckColumnRef(ColumnRefExpression &reference) {
    RequireBuiltin("struct_pack", CatalogType::SCALAR_FUNCTION_ENTRY, "the column reference " + Quote(reference.ToString()));
    if (reference.ColumnNames().size() == 1) {
      static const unordered_map<string, string> value_functions = {
        {"current_catalog", "current_catalog"}, {"current_date", "current_date"},
        {"current_schema", "current_schema"}, {"current_role", "current_role"},
        {"current_time", "get_current_time"}, {"current_timestamp", "get_current_timestamp"},
        {"current_user", "current_user"}, {"localtime", "current_localtime"},
        {"localtimestamp", "current_localtimestamp"}, {"session_user", "session_user"}, {"user", "user"}
      };
      auto value_function = value_functions.find(StringUtil::Lower(reference.GetColumnName().GetIdentifierName()));
      if (value_function != value_functions.end()) {
        RequireBuiltin(value_function->second, CatalogType::SCALAR_FUNCTION_ENTRY,
                       "the SQL value function " + Quote(value_function->first));
      }
      return;
    }
    // "a.b" is a relation-qualified column or a field of column a. Values of
    // verified inputs are never JSON, VARIANT, GEOMETRY or UNION (types are
    // checked at every source), so extraction binds struct_extract or
    // map_extract_value.
    RequireStructExtraction(reference.ToString());
  }

  void RequireStructExtraction(const string &display) {
    RequireBuiltin("struct_extract", CatalogType::SCALAR_FUNCTION_ENTRY, "the field access " + Quote(display));
    RequireBuiltin("map_extract_value", CatalogType::SCALAR_FUNCTION_ENTRY, "the field access " + Quote(display));
  }

  void CheckExpression(ParsedExpression &expression) {
    switch (expression.GetExpressionClass()) {
    case ExpressionClass::FUNCTION:
      CheckFunction(expression.Cast<FunctionExpression>(), false);
      return;
    case ExpressionClass::WINDOW:
      CheckWindow(expression.Cast<WindowExpression>());
      return;
    case ExpressionClass::CAST: {
      auto &cast = expression.Cast<CastExpression>();
      // The generic iterator does not visit the target type expression.
      CheckTypeExpression(cast.TargetType());
      CheckExpression(*cast.ChildMutable());
      return;
    }
    case ExpressionClass::SUBQUERY:
      CheckNode(*expression.Cast<SubqueryExpression>().SubqueryMutable()->node);
      break;
    case ExpressionClass::COLUMN_REF:
      CheckColumnRef(expression.Cast<ColumnRefExpression>());
      break;
    case ExpressionClass::OPERATOR:
      switch (expression.GetExpressionType()) {
      case ExpressionType::OPERATOR_IS_NULL:
      case ExpressionType::OPERATOR_IS_NOT_NULL:
      case ExpressionType::OPERATOR_COALESCE:
      case ExpressionType::OPERATOR_NOT:
      case ExpressionType::OPERATOR_TRY:
      case ExpressionType::COMPARE_IN:
      case ExpressionType::COMPARE_NOT_IN:
        break;
      // These operators bind the named scalar function through the search
      // path. Verified inputs are never JSON or VARIANT (see CheckColumnRef).
      case ExpressionType::ARRAY_EXTRACT:
        RequireBuiltin("array_extract", CatalogType::SCALAR_FUNCTION_ENTRY, "the subscript " + Quote(expression.ToString()));
        RequireBuiltin("map_extract_value", CatalogType::SCALAR_FUNCTION_ENTRY, "the subscript " + Quote(expression.ToString()));
        break;
      case ExpressionType::ARRAY_SLICE:
        RequireBuiltin("array_slice", CatalogType::SCALAR_FUNCTION_ENTRY, "the slice " + Quote(expression.ToString()));
        break;
      case ExpressionType::ARRAY_CONSTRUCTOR:
        RequireBuiltin("list_value", CatalogType::SCALAR_FUNCTION_ENTRY, "the list " + Quote(expression.ToString()));
        break;
      case ExpressionType::STRUCT_EXTRACT:
        RequireStructExtraction(expression.ToString());
        break;
      default:
        Refuse("the operator in " + Quote(expression.ToString()));
      }
      break;
    case ExpressionClass::CASE:
    case ExpressionClass::COMPARISON:
    case ExpressionClass::CONJUNCTION:
    case ExpressionClass::CONSTANT:
    case ExpressionClass::STAR:
    case ExpressionClass::POSITIONAL_REFERENCE:
    case ExpressionClass::BETWEEN:
      break;
    case ExpressionClass::LAMBDA:
    case ExpressionClass::LAMBDA_REF:
      Refuse("lambda expressions");
    case ExpressionClass::COLLATE:
      Refuse("COLLATE expressions");
    case ExpressionClass::PARAMETER:
      Refuse("prepared statement parameters");
    default:
      // No permissive default for new AST kinds.
      Refuse("the expression " + Quote(expression.ToString()));
    }
    ParsedExpressionIterator::EnumerateChildren(expression, [this](ParsedExpression &child) { CheckExpression(child); });
  }

  //! Types bind from the catalog in DuckDB 2: a user type can shadow a built-in
  //! name and type parameters are evaluated during binding.
  void CheckTypeExpression(const TypeExpression &type) {
    enum class Parameters { NONE, LENGTH, DECIMAL, LIST, ARRAY, STRUCT, MAP };
    static const unordered_map<string, Parameters> types = {
      {"boolean", Parameters::NONE}, {"bool", Parameters::NONE}, {"logical", Parameters::NONE},
      {"tinyint", Parameters::NONE}, {"int1", Parameters::NONE}, {"smallint", Parameters::NONE},
      {"int2", Parameters::NONE}, {"int16", Parameters::NONE}, {"short", Parameters::NONE},
      {"integer", Parameters::NONE}, {"int", Parameters::NONE}, {"int4", Parameters::NONE},
      {"int32", Parameters::NONE}, {"signed", Parameters::NONE}, {"bigint", Parameters::NONE},
      {"int8", Parameters::NONE}, {"int64", Parameters::NONE}, {"long", Parameters::NONE},
      {"hugeint", Parameters::NONE}, {"int128", Parameters::NONE}, {"utinyint", Parameters::NONE},
      {"uint8", Parameters::NONE}, {"usmallint", Parameters::NONE}, {"uint16", Parameters::NONE},
      {"uinteger", Parameters::NONE}, {"uint32", Parameters::NONE}, {"ubigint", Parameters::NONE},
      {"uint64", Parameters::NONE}, {"uhugeint", Parameters::NONE}, {"uint128", Parameters::NONE},
      {"float", Parameters::NONE}, {"float4", Parameters::NONE}, {"real", Parameters::NONE},
      {"double", Parameters::NONE}, {"float8", Parameters::NONE}, {"date", Parameters::NONE},
      {"time", Parameters::NONE}, {"timestamp", Parameters::NONE}, {"datetime", Parameters::NONE},
      {"timestamp_s", Parameters::NONE}, {"timestamp_ms", Parameters::NONE}, {"timestamp_us", Parameters::NONE},
      {"timestamp_ns", Parameters::NONE}, {"timestamptz", Parameters::NONE},
      {"timestamp with time zone", Parameters::NONE}, {"timetz", Parameters::NONE},
      {"time with time zone", Parameters::NONE}, {"interval", Parameters::NONE}, {"blob", Parameters::NONE},
      {"bytea", Parameters::NONE}, {"binary", Parameters::NONE}, {"varbinary", Parameters::NONE},
      {"uuid", Parameters::NONE}, {"guid", Parameters::NONE},
      {"varchar", Parameters::LENGTH}, {"text", Parameters::LENGTH}, {"string", Parameters::LENGTH},
      {"char", Parameters::LENGTH}, {"bpchar", Parameters::LENGTH}, {"nvarchar", Parameters::LENGTH},
      {"decimal", Parameters::DECIMAL}, {"numeric", Parameters::DECIMAL}, {"dec", Parameters::DECIMAL},
      {"list", Parameters::LIST}, {"array", Parameters::ARRAY}, {"struct", Parameters::STRUCT},
      {"row", Parameters::STRUCT}, {"map", Parameters::MAP}
    };
    const auto &type_name = type.GetTypeName();
    const auto display = "casts to the type " + Quote(type.ToString());
    if (type.GetQualifiedName().Path().size() != 1) Refuse(display);
    // Filter by name before any lookup: DuckDB autoloads extensions for some type names.
    auto kind = types.find(StringUtil::Lower(type_name.GetIdentifierName()));
    if (kind == types.end()) Refuse(display);
    // Type parameters are folded during binding: only literals and nested
    // built-in types, in the shape each type takes. ARRAY is (type[, size]).
    auto &children = type.GetChildren();
    for (idx_t i = 0; i < children.size(); i++) {
      bool nested_type = false;
      switch (kind->second) {
      case Parameters::NONE: Refuse(display);
      case Parameters::LENGTH:
      case Parameters::DECIMAL: nested_type = false; break;
      case Parameters::LIST:
      case Parameters::STRUCT:
      case Parameters::MAP: nested_type = true; break;
      case Parameters::ARRAY: nested_type = i == 0; break;
      }
      auto expected = nested_type ? ExpressionClass::TYPE : ExpressionClass::CONSTANT;
      if (children[i]->GetExpressionClass() != expected) Refuse(display);
      // Integer literals only: a VARCHAR string parameter selects a collation.
      if (!nested_type && children[i]->Cast<ConstantExpression>().GetLiteral().kind != LiteralKind::INTEGER) {
        Refuse(display);
      }
      if (nested_type) CheckTypeExpression(children[i]->Cast<TypeExpression>());
    }
    bool shape = false;
    switch (kind->second) {
    case Parameters::NONE: shape = children.empty(); break;
    case Parameters::LENGTH: shape = children.size() <= 1; break;
    case Parameters::DECIMAL: shape = children.size() <= 2; break;
    case Parameters::LIST: shape = children.size() == 1; break;
    case Parameters::ARRAY: shape = children.size() == 1 || children.size() == 2; break;
    case Parameters::STRUCT: shape = !children.empty(); break;
    case Parameters::MAP: shape = children.size() == 2; break;
    }
    if (!shape) Refuse(display);
    // Built-in types are generated, internal entries in each DuckDB catalog's
    // main schema; a user type that shadows the name is never internal.
    auto entry = ResolveType(type);
    if (!entry || !entry->internal || entry->Cast<TypeCatalogEntry>().user_type.HasAlias()) {
      Refuse("casts to the user-defined type " + Quote(type.ToString()));
    }
  }

  //! Mirrors ExpressionBinder::BindExpression(TypeExpression &) for an
  //! unqualified type name.
  optional_ptr<CatalogEntry> ResolveType(const TypeExpression &type) {
    auto valid = [](optional_ptr<CatalogEntry> entry) {
      return entry && entry->type == CatalogType::TYPE_ENTRY &&
             entry->Cast<TypeCatalogEntry>().user_type.id() != LogicalTypeId::INVALID;
    };
    try {
      const auto &type_name = type.GetTypeName();
      EntryLookupInfo type_lookup(CatalogType::TYPE_ENTRY, QualifiedName(type_name));
      auto bound_name = Binder::BindTableName(Retriever(), type.GetQualifiedName());
      auto &type_catalog = bound_name.Catalog();
      if (type_catalog.empty() && !DatabaseManager::Get(context).HasAttachedDatabase()) {
        auto entry = Retriever().GetEntry(EntryLookupInfo(type_lookup, bound_name.WithCatalog(Identifier::SystemCatalog())),
                                          OnEntryNotFound::RETURN_NULL);
        return valid(entry) ? entry : nullptr;
      }
      auto entry = Retriever().GetEntry(EntryLookupInfo(type_lookup, bound_name), OnEntryNotFound::RETURN_NULL);
      if (!valid(entry)) {
        entry = Retriever().GetEntry(
          EntryLookupInfo(type_lookup, QualifiedName(type_catalog, Identifier::InvalidSchema(), type_name)),
          OnEntryNotFound::RETURN_NULL);
      }
      if (!valid(entry)) {
        entry = Retriever().GetEntry(
          EntryLookupInfo(type_lookup, QualifiedName(Identifier::InvalidCatalog(), Identifier::InvalidSchema(), type_name)),
          OnEntryNotFound::RETURN_NULL);
      }
      if (!valid(entry)) {
        entry = Retriever().GetEntry(
          EntryLookupInfo(type_lookup, QualifiedName(Identifier::SystemCatalog(), Identifier::DefaultSchema(), type_name)),
          OnEntryNotFound::RETURN_NULL);
      }
      return valid(entry) ? entry : nullptr;
    } catch (const Exception &) {
      return nullptr;
    }
  }

  static void CheckType(const LogicalType &type, const std::function<void()> &refuse) {
    // Aliased/custom types can introduce user-defined implicit casts, even when
    // the SQL itself contains no explicit function or CAST expression.
    if (type.HasAlias()) refuse();
    switch (type.id()) {
    case LogicalTypeId::BOOLEAN:
    case LogicalTypeId::TINYINT:
    case LogicalTypeId::SMALLINT:
    case LogicalTypeId::INTEGER:
    case LogicalTypeId::BIGINT:
    case LogicalTypeId::HUGEINT:
    case LogicalTypeId::UTINYINT:
    case LogicalTypeId::USMALLINT:
    case LogicalTypeId::UINTEGER:
    case LogicalTypeId::UBIGINT:
    case LogicalTypeId::UHUGEINT:
    case LogicalTypeId::FLOAT:
    case LogicalTypeId::DOUBLE:
    case LogicalTypeId::DECIMAL:
    case LogicalTypeId::DATE:
    case LogicalTypeId::TIME:
    case LogicalTypeId::TIMESTAMP:
    case LogicalTypeId::TIMESTAMP_SEC:
    case LogicalTypeId::TIMESTAMP_MS:
    case LogicalTypeId::TIMESTAMP_NS:
    case LogicalTypeId::TIMESTAMP_TZ:
    case LogicalTypeId::TIME_TZ:
    case LogicalTypeId::INTERVAL:
    case LogicalTypeId::BLOB:
    case LogicalTypeId::UUID:
      return;
    case LogicalTypeId::VARCHAR:
      if (!StringType::GetCollation(type).empty()) refuse();
      return;
    case LogicalTypeId::LIST:
      CheckType(ListType::GetChildType(type), refuse);
      return;
    case LogicalTypeId::ARRAY:
      CheckType(ArrayType::GetChildType(type), refuse);
      return;
    case LogicalTypeId::STRUCT:
      for (const auto &child : StructType::GetChildTypes(type)) CheckType(child.second, refuse);
      return;
    case LogicalTypeId::MAP:
      CheckType(MapType::KeyType(type), refuse);
      CheckType(MapType::ValueType(type), refuse);
      return;
    default:
      refuse();
    }
  }

  bool InCTEScope(const Identifier &name) const {
    for (idx_t i = cte_floor; i < cte_names.size(); i++) {
      if (cte_names[i] == name) return true;
    }
    return false;
  }

  //! Mirrors Binder::Bind(BaseTableRef &) for the catalog lookup.
  optional_ptr<CatalogEntry> LookupRelation(BaseTableRef &table) {
    try {
      EntryLookupInfo table_lookup(CatalogType::TABLE_ENTRY, QualifiedName(table.Table()));
      auto bound_name = Binder::BindTableName(Retriever(), table.GetQualifiedName());
      return Retriever().GetEntry(EntryLookupInfo(table_lookup, bound_name), OnEntryNotFound::RETURN_NULL);
    } catch (const Exception &) {
      Refuse(Quote(table.GetQualifiedName().ToString()) + ", which could not be resolved");
    }
  }

  void CheckRelation(CatalogEntry &entry, const string &display) {
    auto catalog_type = entry.ParentCatalog().GetCatalogType();
    if (catalog_type != "duckdb") {
      Refuse(display + " from an attached " + Quote(catalog_type) + " database (only native DuckDB tables are previewed)");
    }
    if (entry.type == CatalogType::TABLE_ENTRY) {
      auto &table = entry.Cast<TableCatalogEntry>();
      // Generated columns are expressions DuckDB binds from the table definition.
      if (table.HasGeneratedColumns()) Refuse("the table " + display + ", which has generated columns");
      for (const auto &column : table.GetColumns().Logical()) {
        CheckType(column.Type(), [&]() {
          Refuse("the column " + Quote(column.Name().GetIdentifierName()) + " of " + display + " with type " +
                 column.Type().ToString());
        });
      }
      return;
    }
    if (entry.type == CatalogType::VIEW_ENTRY) {
      CheckView(entry.Cast<ViewCatalogEntry>(), display);
      return;
    }
    Refuse(display + ", which is not a table or view");
  }

  //! Applies this policy to the view's definition, binding names the way
  //! DuckDB binds view bodies: the view's own catalog and schema first, and no
  //! CTEs from the referencing query.
  void CheckView(ViewCatalogEntry &view, const string &display) {
    if (view_path.size() >= MAX_VIEW_DEPTH) {
      Refuse("the view " + display + ", which nests views more than " + to_string(MAX_VIEW_DEPTH) + " levels deep");
    }
    if (++view_expansions > MAX_VIEW_EXPANSIONS) {
      Refuse("statements that expand more than " + to_string(MAX_VIEW_EXPANSIONS) + " views");
    }
    auto query = unique_ptr_cast<SQLStatement, SelectStatement>(view.GetQuery().Copy());
    // Binder::GetSearchPath(view catalog, view schema, true), as Bind(BaseTableRef &) applies it.
    vector<CatalogSearchEntry> search_path;
    search_path.emplace_back(view.ParentCatalog().GetName(), view.ParentSchema().name);
    search_path.emplace_back(view.ParentCatalog().GetName(), Identifier::InvalidSchema(), true);
    auto retriever = make_uniq<CatalogEntryRetriever>(context);
    retriever->Inherit(Retriever());
    retriever->SetSearchPath(std::move(search_path));
    retrievers.push_back(std::move(retriever));
    const auto previous_floor = cte_floor;
    cte_floor = cte_names.size();
    view_path.push_back(display);
    CheckNode(*query->node);
    view_path.pop_back();
    cte_floor = previous_floor;
    retrievers.pop_back();
  }

  void CheckBaseTable(BaseTableRef &table) {
    // DuckDB evaluates AT expressions during binding, even when the catalog
    // subsequently rejects time travel.
    if (table.at_clause) Refuse("AT (time travel) clauses");
    const auto &name = table.GetQualifiedName();
    const auto display = Quote(name.ToString());
    if (name.Path().size() == 1 && InCTEScope(table.Table())) {
      // DuckDB binds the innermost visible CTE first. If a relation with the
      // same name exists, require it to be acceptable as well, so the
      // decision never depends on that precedence.
      if (auto entry = LookupRelation(table)) CheckRelation(*entry, display);
      return;
    }
    // Outside views, the statement is bound beneath the caller's query, whose
    // CTEs would take precedence over the catalog.
    if (view_path.empty() && outer_binder && outer_binder->GetCTEBinding(BindingAlias(name.Schema(), table.Table()))) {
      Refuse(display + ", which names a CTE of the surrounding query");
    }
    // Missing relations are refused before replacement scans (including files
    // and URLs) can run.
    auto entry = LookupRelation(table);
    if (!entry) {
      Refuse(display + ", which is not a table or view in an attached DuckDB database (file, URL and replacement "
             "scans need Run)");
    }
    CheckRelation(*entry, display);
  }

  void CheckTable(TableRef &reference) {
    switch (reference.type) {
    case TableReferenceType::EMPTY_FROM:
      return;
    case TableReferenceType::BASE_TABLE:
      CheckBaseTable(reference.Cast<BaseTableRef>());
      return;
    case TableReferenceType::SUBQUERY:
      CheckNode(*reference.Cast<SubqueryRef>().subquery->node);
      return;
    case TableReferenceType::JOIN: {
      auto &join = reference.Cast<JoinRef>();
      CheckTable(*join.left);
      CheckTable(*join.right);
      if (join.condition) CheckExpression(*join.condition);
      if (join.ranking_expression) CheckExpression(*join.ranking_expression);
      for (auto &expression : join.duplicate_eliminated_columns) CheckExpression(*expression);
      return;
    }
    case TableReferenceType::EXPRESSION_LIST:
      for (auto &row : reference.Cast<ExpressionListRef>().values) {
        for (auto &expression : row) CheckExpression(*expression);
      }
      return;
    case TableReferenceType::TABLE_FUNCTION: {
      auto &table = reference.Cast<TableFunctionRef>();
      if (!table.function || table.function->GetExpressionClass() != ExpressionClass::FUNCTION || table.subquery) {
        Refuse("this table function call");
      }
      CheckFunction(table.function->Cast<FunctionExpression>(), true);
      return;
    }
    case TableReferenceType::PIVOT:
      // PIVOT can run a query during binding to find its columns.
      Refuse("PIVOT and UNPIVOT");
    case TableReferenceType::SHOW_REF:
      Refuse("DESCRIBE, SHOW and SUMMARIZE");
    default:
      Refuse("this kind of FROM clause");
    }
  }

  void CheckNode(QueryNode &node) {
    // CTEs are visible to later CTEs of the same WITH clause, to the query and
    // to its subqueries. A CTE never sees itself or later siblings here, so this
    // scope is never wider than DuckDB's.
    const auto scope_size = cte_names.size();
    for (auto &cte : node.cte_map.map) {
      auto &info = *cte.second;
      if (!info.key_targets.empty() || !info.payload_aggregates.empty()) Refuse("recursive CTEs");
      if (!info.query_node) Refuse("this WITH clause");
      CheckNode(*info.query_node);
      cte_names.push_back(cte.first);
    }
    switch (node.type) {
    case QueryNodeType::SELECT_NODE: {
      auto &select = node.Cast<SelectNode>();
      for (auto &expression : select.select_list) CheckExpression(*expression);
      for (auto &expression : select.groups.group_expressions) CheckExpression(*expression);
      if (select.where_clause) CheckExpression(*select.where_clause);
      if (select.having) CheckExpression(*select.having);
      if (select.qualify) CheckExpression(*select.qualify);
      if (select.from_table) CheckTable(*select.from_table);
      break;
    }
    case QueryNodeType::SET_OPERATION_NODE:
      for (auto &child : node.Cast<SetOperationNode>().children) CheckNode(*child);
      break;
    case QueryNodeType::RECURSIVE_CTE_NODE:
      // A recursive CTE can run unbounded before the row limit applies.
      Refuse("recursive CTEs");
    case QueryNodeType::INSERT_QUERY_NODE:
    case QueryNodeType::UPDATE_QUERY_NODE:
    case QueryNodeType::DELETE_QUERY_NODE:
    case QueryNodeType::MERGE_QUERY_NODE:
      Refuse("INSERT, UPDATE, DELETE or MERGE inside a query");
    case QueryNodeType::COPY_QUERY_NODE:
      Refuse("COPY inside a query");
    default:
      Refuse("this query form");
    }
    ParsedExpressionIterator::EnumerateQueryNodeModifiers(node,
      [this](unique_ptr<ParsedExpression> &expression) { CheckExpression(*expression); });
    cte_names.resize(scope_size);
  }

  ClientContext &context;
  optional_ptr<Binder> outer_binder;
  //! The innermost entry resolves names: the caller's search path, then one per view being verified.
  vector<unique_ptr<CatalogEntryRetriever>> retrievers;
  //! CTE names visible at the current point; entries below cte_floor belong to a query outside the current view.
  vector<Identifier> cte_names;
  idx_t cte_floor = 0;
  vector<string> view_path;
  idx_t view_expansions = 0;
};

static void IsReadOnly(DataChunk &input, ExpressionState &state, Vector &result) {
  UnaryExecutor::Execute<string_t, bool>(input.data[0], result, input.size(), [&](string_t sql) {
    try {
      Policy(state.GetContext()).Parse(sql.GetString());
      return true;
    } catch (const Exception &) {
      return false;
    }
  });
}

static unique_ptr<TableRef> ReadQuery(ClientContext &context, TableFunctionBindInput &input) {
  if (input.inputs[0].IsNull()) throw InvalidInputException("SQLNB_READ_ONLY: SQL cannot be null");
  // Validate and return the same parsed tree, in the outer query's binding
  // transaction. A separate prior classification cannot authorize this path.
  return make_uniq<SubqueryRef>(Policy(context, input.binder).Parse(input.inputs[0].GetValue<string>()));
}

} // namespace sqlnb_read_policy

static void RegisterSQLNBReadPolicy(ExtensionLoader &loader) {
  ScalarFunction policy("sqlnb_is_read_only", {LogicalType::VARCHAR}, LogicalType::BOOLEAN, sqlnb_read_policy::IsReadOnly);
  policy.SetVolatile();
  loader.RegisterFunction(policy);
  TableFunction read("sqlnb_read_query", {LogicalType::VARCHAR}, nullptr, nullptr);
  read.bind_replace = sqlnb_read_policy::ReadQuery;
  loader.RegisterFunction(read);
}
} // namespace duckdb
#endif
