#pragma once

// This policy deliberately accepts a subset of SQL. It parses without extension
// hooks and never PREPAREs, EXPLAINs or executes unverified SQL to classify it.
// Unknown constructs remain available through explicit Run, never Instant SQL.
#if __has_include("duckdb/main/query_result_stream.hpp")
#include "duckdb/catalog/catalog.hpp"
#include "duckdb/catalog/catalog_entry/table_catalog_entry.hpp"
#include "duckdb/catalog/entry_lookup_info.hpp"
#include "duckdb/function/scalar_function.hpp"
#include "duckdb/function/table_function.hpp"
#include "duckdb/main/extension/extension_loader.hpp"
#include "duckdb/parser/parser.hpp"
#include "duckdb/parser/column_list.hpp"
#include "duckdb/parser/expression/columnref_expression.hpp"
#include "duckdb/parser/common_table_expression_info.hpp"
#include "duckdb/parser/parsed_expression_iterator.hpp"
#include "duckdb/parser/expression/function_expression.hpp"
#include "duckdb/parser/expression/subquery_expression.hpp"
#include "duckdb/parser/query_node/select_node.hpp"
#include "duckdb/parser/query_node/set_operation_node.hpp"
#include "duckdb/parser/statement/select_statement.hpp"
#include "duckdb/parser/tableref/basetableref.hpp"
#include "duckdb/parser/tableref/expressionlistref.hpp"
#include "duckdb/parser/tableref/joinref.hpp"
#include "duckdb/parser/tableref/subqueryref.hpp"
#include "duckdb/parser/tableref/table_function_ref.hpp"

namespace duckdb {
namespace sqlnb_read_policy {

class Policy {
public:
  explicit Policy(ClientContext &context_p) : context(context_p) {}

  unique_ptr<SelectStatement> Parse(const string &sql) {
    auto parser = Parser::GetBuiltinParser();
    parser.ParseQuery(sql);
    if (parser.statements.size() != 1 || parser.statements[0]->type != StatementType::SELECT_STATEMENT) {
      Reject();
    }
    auto statement = unique_ptr_cast<SQLStatement, SelectStatement>(std::move(parser.statements[0]));
    CheckNode(*statement->node);
    return statement;
  }

private:
  [[noreturn]] static void Reject() {
    throw InvalidInputException("SQLNB_READ_ONLY: this statement cannot be verified for Instant SQL; use Run explicitly");
  }

  void CheckFunction(FunctionExpression &function, bool table_function) {
    const auto name = StringUtil::Lower(function.FunctionName().GetIdentifierName());
    // Names are only a first filter. Resolve the actual entry to exclude user
    // macros/UDFs shadowing an allowed built-in and extension-provided overloads.
    static const unordered_set<string> scans = {"range", "generate_series", "unnest", "repeat"};
    static const unordered_set<string> scalars = {
      "+", "-", "*", "/", "//", "%", "**", "||", "abs", "ceil", "ceiling", "floor", "round", "trunc",
      "sqrt", "pow", "power", "exp", "ln", "log", "log10", "sign", "greatest", "least",
      "lower", "upper", "length", "char_length", "octet_length", "trim", "ltrim", "rtrim", "substring",
      "substr", "left", "right", "replace", "repeat", "concat", "concat_ws", "starts_with", "ends_with", "contains",
      "like_escape", "not_like_escape", "ilike_escape", "not_ilike_escape", "regexp_matches", "regexp_full_match",
      "count", "count_star", "sum", "avg", "min", "max", "median", "stddev", "stddev_samp", "stddev_pop",
      "variance", "var_samp", "var_pop", "bool_and", "bool_or", "first", "last", "any_value", "string_agg",
      "list", "array_agg", "list_value", "struct_pack", "struct_extract", "array_extract", "list_extract",
      "array_slice", "list_slice", "array_length", "list_count", "list_sum", "list_min", "list_max",
      "date_part", "date_trunc", "date_diff", "datediff", "strftime", "strptime", "try_strptime"
    };
    if (!(table_function ? scans : scalars).count(name)) Reject();
    auto entry = Catalog::GetEntry(context,
        EntryLookupInfo(table_function ? CatalogType::TABLE_FUNCTION_ENTRY : CatalogType::SCALAR_FUNCTION_ENTRY,
                        function.GetQualifiedName()), OnEntryNotFound::RETURN_NULL);
    if (!entry || !entry->internal ||
        (entry->type != CatalogType::SCALAR_FUNCTION_ENTRY && entry->type != CatalogType::AGGREGATE_FUNCTION_ENTRY &&
         entry->type != CatalogType::TABLE_FUNCTION_ENTRY)) Reject();
    auto extension = entry->extension_name.GetIdentifierName();
    if (!extension.empty() && extension != "core_functions") Reject();
    ParsedExpressionIterator::EnumerateChildren(function, [this](ParsedExpression &child) { CheckExpression(child); });
  }

  void CheckExpression(ParsedExpression &expression) {
    switch (expression.GetExpressionClass()) {
    case ExpressionClass::FUNCTION:
      CheckFunction(expression.Cast<FunctionExpression>(), false);
      return;
    case ExpressionClass::SUBQUERY:
      CheckNode(*expression.Cast<SubqueryExpression>().SubqueryMutable()->node);
      break;
    case ExpressionClass::COLUMN_REF:
      // A dotted reference can be a relation-qualified column or an implicit
      // struct/JSON extraction that binds a scalar through the search path.
      // Without binding we cannot distinguish them safely.
      if (expression.Cast<ColumnRefExpression>().ColumnNames().size() != 1) Reject();
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
      default:
        // Array/struct extraction and other operators can dispatch to hidden
        // scalar calls. Require an explicitly checked function call instead.
        Reject();
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
    default:
      // Casts, collations, lambdas and window functions can resolve additional
      // user-defined code at bind time. No permissive default for new AST kinds.
      Reject();
    }
    ParsedExpressionIterator::EnumerateChildren(expression, [this](ParsedExpression &child) { CheckExpression(child); });
  }

  static void CheckType(const LogicalType &type) {
    // Aliased/custom types can introduce user-defined implicit casts, even when
    // the SQL itself contains no explicit function or CAST expression.
    if (type.HasAlias()) Reject();
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
      if (!StringType::GetCollation(type).empty()) Reject();
      return;
    case LogicalTypeId::LIST:
      CheckType(ListType::GetChildType(type));
      return;
    case LogicalTypeId::ARRAY:
      CheckType(ArrayType::GetChildType(type));
      return;
    case LogicalTypeId::STRUCT:
      for (const auto &child : StructType::GetChildTypes(type)) CheckType(child.second);
      return;
    case LogicalTypeId::MAP:
      CheckType(MapType::KeyType(type));
      CheckType(MapType::ValueType(type));
      return;
    default:
      Reject();
    }
  }

  void CheckTable(TableRef &reference) {
    switch (reference.type) {
    case TableReferenceType::EMPTY_FROM:
      return;
    case TableReferenceType::BASE_TABLE: {
      auto &table = reference.Cast<BaseTableRef>();
      // DuckDB evaluates AT expressions during binding, even when the catalog
      // subsequently rejects time travel. Native catalogs admitted here do not
      // support it, so reject before any hidden expression can execute.
      if (table.at_clause) Reject();
      // Missing tables are rejected before replacement scans (including files)
      // can run. Views and generated columns can conceal calls: fail closed.
      auto entry = Catalog::GetEntry(context, EntryLookupInfo(CatalogType::TABLE_ENTRY, table.GetQualifiedName()),
                                    OnEntryNotFound::RETURN_NULL);
      if (!entry || entry->type != CatalogType::TABLE_ENTRY || entry->ParentCatalog().GetCatalogType() != "duckdb" ||
          entry->Cast<TableCatalogEntry>().HasGeneratedColumns()) Reject();
      for (const auto &column : entry->Cast<TableCatalogEntry>().GetColumns().Logical()) CheckType(column.Type());
      return;
    }
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
      if (!table.function || table.function->GetExpressionClass() != ExpressionClass::FUNCTION || table.subquery) Reject();
      CheckFunction(table.function->Cast<FunctionExpression>(), true);
      return;
    }
    default:
      Reject();
    }
  }

  void CheckNode(QueryNode &node) {
    // CTEs can contain modifying query nodes in DuckDB 2. Keep them explicit
    // until their lexical scope and every node kind have a dedicated policy.
    if (!node.cte_map.map.empty()) Reject();
    if (node.type == QueryNodeType::SELECT_NODE) {
      auto &select = node.Cast<SelectNode>();
      for (auto &expression : select.select_list) CheckExpression(*expression);
      for (auto &expression : select.groups.group_expressions) CheckExpression(*expression);
      if (select.where_clause) CheckExpression(*select.where_clause);
      if (select.having) CheckExpression(*select.having);
      if (select.qualify) CheckExpression(*select.qualify);
      if (select.from_table) CheckTable(*select.from_table);
    } else if (node.type == QueryNodeType::SET_OPERATION_NODE) {
      for (auto &child : node.Cast<SetOperationNode>().children) CheckNode(*child);
    } else Reject();
    ParsedExpressionIterator::EnumerateQueryNodeModifiers(node,
      [this](unique_ptr<ParsedExpression> &expression) { CheckExpression(*expression); });
  }

  ClientContext &context;
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
  return make_uniq<SubqueryRef>(Policy(context).Parse(input.inputs[0].GetValue<string>()));
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
