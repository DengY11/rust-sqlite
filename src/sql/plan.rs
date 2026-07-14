use crate::common::types::{ColumnDef, Value};
use crate::sql::ast::{
    AlterTableAction, Assignment, CompareOp, CompoundOperator, Expr, FromItem, IsolationLevel,
    JoinKind, OrderBy, ScalarExpr, SelectItem, TableConstraint, UpsertClause,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JoinPlan {
    pub kind: JoinKind,
    pub source: Box<Plan>,
    pub on: Expr,
    pub using_columns: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexScanSpec {
    pub index: String,
    pub mode: IndexScanMode,
    pub key_prefix: Vec<Value>,
    pub range: Option<IndexRange>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexScanMode {
    Lookup,
    Prefix,
    Range,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexBound {
    pub op: CompareOp,
    pub value: Value,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexRange {
    pub column: String,
    pub lower: Option<IndexBound>,
    pub upper: Option<IndexBound>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Plan {
    CreateTable {
        name: String,
        columns: Vec<ColumnDef>,
        constraints: Vec<TableConstraint>,
        strict: bool,
        without_rowid: bool,
        if_not_exists: bool,
        temporary: bool,
    },
    CreateTableAs {
        name: String,
        if_not_exists: bool,
        source: Box<Plan>,
        temporary: bool,
    },
    CreateView {
        name: String,
        columns: Vec<ColumnDef>,
        view_columns: Option<Vec<String>>,
        select: crate::sql::ast::SelectStatement,
        create_sql: String,
        temporary: bool,
    },
    CreateIndex {
        name: String,
        table: String,
        columns: Vec<String>,
        decorated_columns: Option<Vec<String>>,
        unique: bool,
        predicate: Option<String>,
        if_not_exists: bool,
    },
    CreateTrigger {
        name: String,
        table: String,
        sql: String,
        if_not_exists: bool,
    },
    DropTable {
        name: String,
        if_exists: bool,
    },
    DropView {
        name: String,
        if_exists: bool,
    },
    DropIndex {
        table: String,
        name: String,
        if_exists: bool,
    },
    DropTrigger {
        name: String,
        if_exists: bool,
    },
    NoOp,
    AlterTable {
        table: String,
        action: AlterTableAction,
    },
    Insert {
        table: String,
        or_conflict: Option<String>,
        values: Vec<Value>,
    },
    InsertReturning {
        table: String,
        or_conflict: Option<String>,
        values: Vec<Value>,
        returning: Vec<SelectItem>,
    },
    InsertUpsert {
        table: String,
        values: Vec<Value>,
        upsert: UpsertClause,
    },
    InsertUpsertReturning {
        table: String,
        values: Vec<Value>,
        upsert: UpsertClause,
        returning: Vec<SelectItem>,
    },
    InsertMany {
        table: String,
        or_conflict: Option<String>,
        rows: Vec<Vec<Value>>,
    },
    InsertManyReturning {
        table: String,
        or_conflict: Option<String>,
        rows: Vec<Vec<Value>>,
        returning: Vec<SelectItem>,
    },
    InsertManyUpsert {
        table: String,
        rows: Vec<Vec<Value>>,
        upsert: UpsertClause,
    },
    InsertManyUpsertReturning {
        table: String,
        rows: Vec<Vec<Value>>,
        upsert: UpsertClause,
        returning: Vec<SelectItem>,
    },
    InsertDoNothing {
        table: String,
        target: Option<Vec<String>>,
        values: Vec<Value>,
    },
    InsertDoNothingReturning {
        table: String,
        target: Option<Vec<String>>,
        values: Vec<Value>,
        returning: Vec<SelectItem>,
    },
    InsertManyDoNothing {
        table: String,
        target: Option<Vec<String>>,
        rows: Vec<Vec<Value>>,
    },
    InsertManyDoNothingReturning {
        table: String,
        target: Option<Vec<String>>,
        rows: Vec<Vec<Value>>,
        returning: Vec<SelectItem>,
    },
    InsertExpr {
        table: String,
        or_conflict: Option<String>,
        values: Vec<ScalarExpr>,
    },
    InsertExprReturning {
        table: String,
        or_conflict: Option<String>,
        values: Vec<ScalarExpr>,
        returning: Vec<SelectItem>,
    },
    InsertExprUpsert {
        table: String,
        values: Vec<ScalarExpr>,
        upsert: UpsertClause,
    },
    InsertExprUpsertReturning {
        table: String,
        values: Vec<ScalarExpr>,
        upsert: UpsertClause,
        returning: Vec<SelectItem>,
    },
    InsertManyExpr {
        table: String,
        or_conflict: Option<String>,
        rows: Vec<Vec<ScalarExpr>>,
    },
    InsertManyExprReturning {
        table: String,
        or_conflict: Option<String>,
        rows: Vec<Vec<ScalarExpr>>,
        returning: Vec<SelectItem>,
    },
    InsertManyExprUpsert {
        table: String,
        rows: Vec<Vec<ScalarExpr>>,
        upsert: UpsertClause,
    },
    InsertManyExprUpsertReturning {
        table: String,
        rows: Vec<Vec<ScalarExpr>>,
        upsert: UpsertClause,
        returning: Vec<SelectItem>,
    },
    InsertExprDoNothing {
        table: String,
        target: Option<Vec<String>>,
        values: Vec<ScalarExpr>,
    },
    InsertExprDoNothingReturning {
        table: String,
        target: Option<Vec<String>>,
        values: Vec<ScalarExpr>,
        returning: Vec<SelectItem>,
    },
    InsertManyExprDoNothing {
        table: String,
        target: Option<Vec<String>>,
        rows: Vec<Vec<ScalarExpr>>,
    },
    InsertManyExprDoNothingReturning {
        table: String,
        target: Option<Vec<String>>,
        rows: Vec<Vec<ScalarExpr>>,
        returning: Vec<SelectItem>,
    },
    InsertSelect {
        table: String,
        columns: Option<Vec<String>>,
        or_conflict: Option<String>,
        source: Box<Plan>,
    },
    InsertSelectReturning {
        table: String,
        columns: Option<Vec<String>>,
        or_conflict: Option<String>,
        source: Box<Plan>,
        returning: Vec<SelectItem>,
    },
    InsertSelectUpsert {
        table: String,
        columns: Option<Vec<String>>,
        source: Box<Plan>,
        upsert: UpsertClause,
    },
    InsertSelectUpsertReturning {
        table: String,
        columns: Option<Vec<String>>,
        source: Box<Plan>,
        upsert: UpsertClause,
        returning: Vec<SelectItem>,
    },
    InsertSelectDoNothing {
        table: String,
        columns: Option<Vec<String>>,
        target: Option<Vec<String>>,
        source: Box<Plan>,
    },
    InsertSelectDoNothingReturning {
        table: String,
        columns: Option<Vec<String>>,
        target: Option<Vec<String>>,
        source: Box<Plan>,
        returning: Vec<SelectItem>,
    },
    Delete {
        table: String,
        filter: Option<Expr>,
    },
    DeleteLimited {
        table: String,
        filter: Option<Expr>,
        order_by: Vec<OrderBy>,
        limit: Option<usize>,
        offset: Option<usize>,
    },
    DeleteReturning {
        table: String,
        filter: Option<Expr>,
        returning: Vec<SelectItem>,
    },
    DeleteReturningLimited {
        table: String,
        filter: Option<Expr>,
        returning: Vec<SelectItem>,
        order_by: Vec<OrderBy>,
        limit: Option<usize>,
        offset: Option<usize>,
    },
    Update {
        table: String,
        or_conflict: Option<String>,
        assignments: Vec<Assignment>,
        filter: Option<Expr>,
    },
    UpdateFrom {
        table: String,
        table_alias: Option<String>,
        source: FromItem,
        or_conflict: Option<String>,
        assignments: Vec<Assignment>,
        filter: Option<Expr>,
        returning: Option<Vec<SelectItem>>,
        order_by: Vec<OrderBy>,
        limit: Option<usize>,
        offset: Option<usize>,
    },
    UpdateLimited {
        table: String,
        or_conflict: Option<String>,
        assignments: Vec<Assignment>,
        filter: Option<Expr>,
        order_by: Vec<OrderBy>,
        limit: Option<usize>,
        offset: Option<usize>,
    },
    UpdateReturning {
        table: String,
        or_conflict: Option<String>,
        assignments: Vec<Assignment>,
        filter: Option<Expr>,
        returning: Vec<SelectItem>,
    },
    UpdateReturningLimited {
        table: String,
        or_conflict: Option<String>,
        assignments: Vec<Assignment>,
        filter: Option<Expr>,
        returning: Vec<SelectItem>,
        order_by: Vec<OrderBy>,
        limit: Option<usize>,
        offset: Option<usize>,
    },
    SeqScan {
        table: String,
        table_alias: Option<String>,
        columns: Vec<SelectItem>,
        filter: Option<Expr>,
        order_by: Vec<OrderBy>,
        limit: Option<usize>,
        offset: Option<usize>,
        distinct: bool,
    },
    ForcedSeqScan {
        table: String,
        table_alias: Option<String>,
        columns: Vec<SelectItem>,
        filter: Option<Expr>,
        order_by: Vec<OrderBy>,
        limit: Option<usize>,
        offset: Option<usize>,
        distinct: bool,
    },
    IndexScan {
        table: String,
        table_alias: Option<String>,
        columns: Vec<SelectItem>,
        index: String,
        mode: IndexScanMode,
        key_prefix: Vec<Value>,
        range: Option<IndexRange>,
        filter: Option<Expr>,
        order_by: Vec<OrderBy>,
        limit: Option<usize>,
        offset: Option<usize>,
        distinct: bool,
    },
    IndexUnion {
        table: String,
        table_alias: Option<String>,
        columns: Vec<SelectItem>,
        scans: Vec<IndexScanSpec>,
        filter: Option<Expr>,
        order_by: Vec<OrderBy>,
        limit: Option<usize>,
        offset: Option<usize>,
        distinct: bool,
    },
    Union {
        left: Box<Plan>,
        right: Box<Plan>,
        operator: CompoundOperator,
        all: bool,
        order_by: Vec<OrderBy>,
        limit: Option<usize>,
        offset: Option<usize>,
    },
    DerivedSource {
        source: Box<Plan>,
        alias: String,
        output_columns: Vec<String>,
        columns: Vec<SelectItem>,
        filter: Option<Expr>,
        order_by: Vec<OrderBy>,
        limit: Option<usize>,
        offset: Option<usize>,
        distinct: bool,
    },
    NestedLoopJoin {
        source: Box<Plan>,
        joins: Vec<JoinPlan>,
        columns: Vec<SelectItem>,
        filter: Option<Expr>,
        order_by: Vec<OrderBy>,
        limit: Option<usize>,
        offset: Option<usize>,
        distinct: bool,
    },
    Aggregate {
        source: Box<Plan>,
        columns: Vec<SelectItem>,
        group_by: Vec<ScalarExpr>,
        having: Option<Expr>,
        order_by: Vec<OrderBy>,
        limit: Option<usize>,
        offset: Option<usize>,
    },
    Values {
        rows: Vec<Vec<ScalarExpr>>,
    },
    PragmaTableFunction {
        name: String,
        argument: Option<String>,
    },
    ExplainQueryPlan {
        plan: Box<Plan>,
    },
    PragmaTableInfo {
        table: String,
        schema: Option<String>,
    },
    PragmaTableXInfo {
        table: String,
        schema: Option<String>,
    },
    PragmaTableList {
        table: Option<String>,
        schema: Option<String>,
    },
    PragmaIndexList {
        table: String,
        schema: Option<String>,
    },
    PragmaIndexInfo {
        index: String,
        schema: Option<String>,
    },
    PragmaIndexXInfo {
        index: String,
        schema: Option<String>,
    },
    PragmaForeignKeyList {
        table: String,
        schema: Option<String>,
    },
    PragmaForeignKeyCheck {
        table: Option<String>,
        schema: Option<String>,
    },
    PragmaForeignKeys,
    SetPragmaForeignKeys {
        enabled: bool,
    },
    PragmaDeferForeignKeys,
    SetPragmaDeferForeignKeys {
        enabled: bool,
    },
    PragmaReadUncommitted,
    SetPragmaReadUncommitted {
        enabled: bool,
    },
    PragmaQueryOnly,
    SetPragmaQueryOnly {
        enabled: bool,
    },
    PragmaCountChanges,
    SetPragmaCountChanges {
        enabled: bool,
    },
    PragmaRecursiveTriggers,
    SetPragmaRecursiveTriggers {
        enabled: bool,
    },
    PragmaTrustedSchema,
    SetPragmaTrustedSchema {
        enabled: bool,
    },
    PragmaIgnoreCheckConstraints,
    SetPragmaIgnoreCheckConstraints {
        enabled: bool,
    },
    PragmaEncoding,
    SetPragmaEncoding,
    PragmaCollationList,
    PragmaDataVersion,
    PragmaQuickCheck,
    PragmaIntegrityCheck,
    PragmaFunctionList,
    PragmaCompileOptions,
    PragmaPragmaList,
    PragmaModuleList,
    PragmaStats,
    PragmaJournalMode {
        schema: Option<String>,
    },
    SetPragmaJournalMode {
        mode: String,
        schema: Option<String>,
    },
    PragmaSynchronous {
        schema: Option<String>,
    },
    SetPragmaSynchronous {
        value: i64,
        schema: Option<String>,
    },
    PragmaCacheSize {
        schema: Option<String>,
    },
    SetPragmaCacheSize {
        value: i64,
        schema: Option<String>,
    },
    PragmaCacheSpill,
    SetPragmaCacheSpill {
        value: Option<i64>,
    },
    PragmaTempStore,
    SetPragmaTempStore {
        value: i64,
    },
    PragmaLockingMode {
        schema: Option<String>,
    },
    SetPragmaLockingMode {
        mode: String,
        schema: Option<String>,
    },
    PragmaMmapSize,
    SetPragmaMmapSize {
        value: i64,
    },
    PragmaAutoVacuum,
    SetPragmaAutoVacuum {
        value: Option<i64>,
    },
    PragmaSecureDelete {
        schema: Option<String>,
    },
    SetPragmaSecureDelete {
        value: Option<i64>,
        schema: Option<String>,
    },
    PragmaWalAutocheckpoint,
    SetPragmaWalAutocheckpoint {
        value: Option<i64>,
    },
    PragmaWalCheckpoint,
    PragmaBusyTimeout,
    SetPragmaBusyTimeout {
        value: i64,
    },
    PragmaAnalysisLimit,
    SetPragmaAnalysisLimit {
        value: Option<u32>,
    },
    PragmaJournalSizeLimit,
    SetPragmaJournalSizeLimit {
        value: i64,
    },
    PragmaSoftHeapLimit,
    SetPragmaSoftHeapLimit {
        value: i64,
    },
    PragmaHardHeapLimit,
    SetPragmaHardHeapLimit {
        value: i64,
    },
    PragmaThreads,
    SetPragmaThreads {
        value: Option<u32>,
    },
    PragmaAutomaticIndex,
    SetPragmaAutomaticIndex {
        enabled: bool,
    },
    PragmaCellSizeCheck,
    SetPragmaCellSizeCheck {
        enabled: bool,
    },
    PragmaFullColumnNames,
    SetPragmaFullColumnNames {
        enabled: bool,
    },
    PragmaShortColumnNames,
    SetPragmaShortColumnNames {
        enabled: bool,
    },
    PragmaFullFsync,
    SetPragmaFullFsync {
        enabled: bool,
    },
    PragmaCheckpointFullFsync,
    SetPragmaCheckpointFullFsync {
        enabled: bool,
    },
    PragmaEmptyResultCallbacks,
    SetPragmaEmptyResultCallbacks {
        enabled: bool,
    },
    PragmaCaseSensitiveLike,
    SetPragmaCaseSensitiveLike {
        enabled: bool,
    },
    PragmaReverseUnorderedSelects,
    SetPragmaReverseUnorderedSelects {
        enabled: bool,
    },
    PragmaDatabaseList,
    PragmaPageSize {
        schema: Option<String>,
    },
    SetPragmaPageSize {
        value: u32,
        schema: Option<String>,
    },
    PragmaPageCount {
        schema: Option<String>,
    },
    PragmaMaxPageCount,
    SetPragmaMaxPageCount {
        value: Option<i64>,
    },
    PragmaFreelistCount {
        schema: Option<String>,
    },
    PragmaUserVersion {
        schema: Option<String>,
    },
    SetPragmaUserVersion {
        value: u32,
        schema: Option<String>,
    },
    PragmaApplicationId {
        schema: Option<String>,
    },
    SetPragmaApplicationId {
        value: u32,
        schema: Option<String>,
    },
    PragmaSchemaVersion {
        schema: Option<String>,
    },
    SetPragmaSchemaVersion {
        value: u32,
        schema: Option<String>,
    },
    BeginTxn {
        isolation_level: IsolationLevel,
    },
    CommitTxn,
    RollbackTxn,
    Savepoint {
        name: String,
    },
    RollbackTo {
        name: String,
    },
    Release {
        name: String,
    },
}

#[cfg(test)]
mod tests {
    use crate::common::types::{ColumnDef, ColumnType, Value};
    use crate::sql::ast::{CompareOp, Expr, JoinKind, SelectItem};

    use super::{IndexBound, IndexRange, IndexScanMode, IndexScanSpec, JoinPlan, Plan};

    #[test]
    fn plan_variants_preserve_statement_payloads() {
        let plan = Plan::CreateTable {
            name: "users".to_string(),
            columns: vec![ColumnDef::primary_key("id", ColumnType::Integer)],
            constraints: vec![],
            strict: false,
            without_rowid: false,
            if_not_exists: false,
            temporary: false,
        };
        assert_eq!(
            plan,
            Plan::CreateTable {
                name: "users".to_string(),
                columns: vec![ColumnDef::primary_key("id", ColumnType::Integer)],
                constraints: vec![],
                strict: false,
                without_rowid: false,
                if_not_exists: false,
                temporary: false,
            }
        );
    }

    #[test]
    fn scan_plans_are_comparable_with_filters() {
        let plan = Plan::SeqScan {
            table: "users".to_string(),
            table_alias: None,
            columns: vec![SelectItem::Wildcard],
            filter: Some(Expr::Compare {
                column: "id".to_string(),
                op: CompareOp::Eq,
                value: Value::Integer(9),
            }),
            order_by: vec![],
            limit: None,
            offset: None,
            distinct: false,
        };
        assert_eq!(
            plan,
            Plan::SeqScan {
                table: "users".to_string(),
                table_alias: None,
                columns: vec![SelectItem::Wildcard],
                filter: Some(Expr::Compare {
                    column: "id".to_string(),
                    op: CompareOp::Eq,
                    value: Value::Integer(9),
                }),
                order_by: vec![],
                limit: None,
                offset: None,
                distinct: false,
            }
        );
    }

    #[test]
    fn index_scan_plans_are_comparable_with_range_bounds() {
        let plan = Plan::IndexScan {
            table: "users".to_string(),
            table_alias: None,
            columns: vec![SelectItem::Wildcard],
            index: "idx_users_id_name".to_string(),
            mode: IndexScanMode::Range,
            key_prefix: vec![Value::Integer(7)],
            range: Some(IndexRange {
                column: "name".to_string(),
                lower: Some(IndexBound {
                    op: CompareOp::Gt,
                    value: Value::from("alice"),
                }),
                upper: None,
            }),
            filter: None,
            order_by: vec![],
            limit: None,
            offset: None,
            distinct: false,
        };
        assert_eq!(
            plan,
            Plan::IndexScan {
                table: "users".to_string(),
                table_alias: None,
                columns: vec![SelectItem::Wildcard],
                index: "idx_users_id_name".to_string(),
                mode: IndexScanMode::Range,
                key_prefix: vec![Value::Integer(7)],
                range: Some(IndexRange {
                    column: "name".to_string(),
                    lower: Some(IndexBound {
                        op: CompareOp::Gt,
                        value: Value::from("alice"),
                    }),
                    upper: None,
                }),
                filter: None,
                order_by: vec![],
                limit: None,
                offset: None,
                distinct: false,
            }
        );
    }

    #[test]
    fn index_union_plans_are_comparable_with_scan_specs() {
        let plan = Plan::IndexUnion {
            table: "users".to_string(),
            table_alias: None,
            columns: vec![SelectItem::Wildcard],
            scans: vec![
                IndexScanSpec {
                    index: "idx_users_id".to_string(),
                    mode: IndexScanMode::Lookup,
                    key_prefix: vec![Value::Integer(7)],
                    range: None,
                },
                IndexScanSpec {
                    index: "idx_users_name".to_string(),
                    mode: IndexScanMode::Lookup,
                    key_prefix: vec![Value::from("alice")],
                    range: None,
                },
            ],
            filter: Some(Expr::Or(
                Box::new(Expr::Compare {
                    column: "id".to_string(),
                    op: CompareOp::Eq,
                    value: Value::Integer(7),
                }),
                Box::new(Expr::Compare {
                    column: "name".to_string(),
                    op: CompareOp::Eq,
                    value: Value::from("alice"),
                }),
            )),
            order_by: vec![],
            limit: None,
            offset: None,
            distinct: false,
        };

        assert_eq!(
            plan,
            Plan::IndexUnion {
                table: "users".to_string(),
                table_alias: None,
                columns: vec![SelectItem::Wildcard],
                scans: vec![
                    IndexScanSpec {
                        index: "idx_users_id".to_string(),
                        mode: IndexScanMode::Lookup,
                        key_prefix: vec![Value::Integer(7)],
                        range: None,
                    },
                    IndexScanSpec {
                        index: "idx_users_name".to_string(),
                        mode: IndexScanMode::Lookup,
                        key_prefix: vec![Value::from("alice")],
                        range: None,
                    },
                ],
                filter: Some(Expr::Or(
                    Box::new(Expr::Compare {
                        column: "id".to_string(),
                        op: CompareOp::Eq,
                        value: Value::Integer(7),
                    }),
                    Box::new(Expr::Compare {
                        column: "name".to_string(),
                        op: CompareOp::Eq,
                        value: Value::from("alice"),
                    }),
                )),
                order_by: vec![],
                limit: None,
                offset: None,
                distinct: false,
            }
        );
    }

    #[test]
    fn nested_loop_join_plans_are_comparable() {
        let plan = Plan::NestedLoopJoin {
            source: Box::new(Plan::SeqScan {
                table: "users".to_string(),
                table_alias: Some("u".to_string()),
                columns: vec![SelectItem::Wildcard],
                filter: None,
                order_by: vec![],
                limit: None,
                offset: None,
                distinct: false,
            }),
            joins: vec![JoinPlan {
                kind: JoinKind::Inner,
                source: Box::new(Plan::SeqScan {
                    table: "orders".to_string(),
                    table_alias: Some("o".to_string()),
                    columns: vec![SelectItem::Wildcard],
                    filter: None,
                    order_by: vec![],
                    limit: None,
                    offset: None,
                    distinct: false,
                }),
                on: Expr::CompareColumns {
                    left: "u.id".to_string(),
                    op: CompareOp::Eq,
                    right: "o.user_id".to_string(),
                },
                using_columns: Vec::new(),
            }],
            columns: vec![SelectItem::Column("u.id".to_string())],
            filter: None,
            order_by: vec![],
            limit: None,
            offset: None,
            distinct: false,
        };

        assert_eq!(
            plan,
            Plan::NestedLoopJoin {
                source: Box::new(Plan::SeqScan {
                    table: "users".to_string(),
                    table_alias: Some("u".to_string()),
                    columns: vec![SelectItem::Wildcard],
                    filter: None,
                    order_by: vec![],
                    limit: None,
                    offset: None,
                    distinct: false,
                }),
                joins: vec![JoinPlan {
                    kind: JoinKind::Inner,
                    source: Box::new(Plan::SeqScan {
                        table: "orders".to_string(),
                        table_alias: Some("o".to_string()),
                        columns: vec![SelectItem::Wildcard],
                        filter: None,
                        order_by: vec![],
                        limit: None,
                        offset: None,
                        distinct: false,
                    }),
                    on: Expr::CompareColumns {
                        left: "u.id".to_string(),
                        op: CompareOp::Eq,
                        right: "o.user_id".to_string(),
                    },
                    using_columns: Vec::new(),
                }],
                columns: vec![SelectItem::Column("u.id".to_string())],
                filter: None,
                order_by: vec![],
                limit: None,
                offset: None,
                distinct: false,
            }
        );
    }

    #[test]
    fn derived_source_plans_are_comparable() {
        let plan = Plan::DerivedSource {
            source: Box::new(Plan::SeqScan {
                table: "users".to_string(),
                table_alias: None,
                columns: vec![SelectItem::Column("age".to_string())],
                filter: None,
                order_by: vec![],
                limit: None,
                offset: None,
                distinct: false,
            }),
            alias: "t".to_string(),
            output_columns: vec!["bucket".to_string()],
            columns: vec![SelectItem::Column("bucket".to_string())],
            filter: None,
            order_by: vec![],
            limit: None,
            offset: None,
            distinct: false,
        };

        assert_eq!(
            plan,
            Plan::DerivedSource {
                source: Box::new(Plan::SeqScan {
                    table: "users".to_string(),
                    table_alias: None,
                    columns: vec![SelectItem::Column("age".to_string())],
                    filter: None,
                    order_by: vec![],
                    limit: None,
                    offset: None,
                    distinct: false,
                }),
                alias: "t".to_string(),
                output_columns: vec!["bucket".to_string()],
                columns: vec![SelectItem::Column("bucket".to_string())],
                filter: None,
                order_by: vec![],
                limit: None,
                offset: None,
                distinct: false,
            }
        );
    }
}
