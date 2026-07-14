use crate::common::types::{
    CheckConstraint, ColumnDef, ColumnType, ForeignKey, PrimaryKeyConstraint, UniqueConstraint,
    Value,
};

pub(crate) const SINGLE_ROW_SOURCE_TABLE: &str = "__rustsql_single_row__";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FromItem {
    Table {
        name: String,
        schema: Option<String>,
        alias: Option<String>,
    },
    TableIndexed {
        name: String,
        schema: Option<String>,
        alias: Option<String>,
        index: String,
    },
    TableNotIndexed {
        name: String,
        schema: Option<String>,
        alias: Option<String>,
    },
    Subquery {
        query: Box<SelectStatement>,
        alias: String,
        columns: Option<Vec<String>>,
    },
    Values {
        rows: Vec<Vec<ScalarExpr>>,
        alias: Option<String>,
        columns: Option<Vec<String>>,
    },
    PragmaTableFunction {
        name: String,
        argument: Option<String>,
        alias: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectStatement {
    pub with: Option<WithClause>,
    pub distinct: bool,
    pub columns: Vec<SelectItem>,
    pub from: FromItem,
    pub joins: Vec<JoinClause>,
    pub filter: Option<Expr>,
    pub group_by: Vec<ScalarExpr>,
    pub having: Option<Expr>,
    pub compounds: Vec<CompoundSelect>,
    pub order_by: Vec<OrderBy>,
    pub limit: Option<usize>,
    pub offset: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompoundSelect {
    pub operator: CompoundOperator,
    pub select: Box<SelectStatement>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompoundOperator {
    Union,
    UnionAll,
    Intersect,
    Except,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IsolationLevel {
    ReadCommitted,
    RepeatableRead,
    Serializable,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WithClause {
    pub recursive: bool,
    pub ctes: Vec<CommonTableExpr>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommonTableExpr {
    pub name: String,
    pub columns: Option<Vec<String>>,
    pub query: CteBody,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CteBody {
    Select(Box<SelectStatement>),
    Values(Vec<Vec<ScalarExpr>>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TableIndexHint {
    IndexedBy(String),
    NotIndexed,
}

impl SelectStatement {
    #[must_use]
    pub fn base_table(&self) -> Option<(&str, Option<&str>)> {
        match &self.from {
            FromItem::Table { name, alias, .. }
            | FromItem::TableIndexed { name, alias, .. }
            | FromItem::TableNotIndexed { name, alias, .. } => {
                Some((name.as_str(), alias.as_deref()))
            }
            FromItem::Subquery { .. }
            | FromItem::Values { .. }
            | FromItem::PragmaTableFunction { .. } => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JoinKind {
    Inner,
    Left,
    Right,
    Full,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JoinClause {
    pub kind: JoinKind,
    pub source: FromItem,
    pub on: Expr,
    pub using_columns: Vec<String>,
    pub natural: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Assignment {
    pub column: String,
    pub value: ScalarExpr,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpsertClause {
    pub target: Option<Vec<String>>,
    pub assignments: Vec<Assignment>,
    pub filter: Option<Expr>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrderBy {
    pub expr: OrderByExpr,
    pub collation: Option<String>,
    pub descending: bool,
    pub nulls: Option<NullOrder>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NullOrder {
    First,
    Last,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OrderByExpr {
    Column(String),
    Position(usize),
    Expr(ScalarExpr),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AlterTableAction {
    AddColumn(ColumnDef),
    RenameTable { new_name: String },
    RenameColumn { old_name: String, new_name: String },
    DropColumn { old_name: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AggregateFunc {
    Count,
    Sum,
    DecimalSum,
    Avg,
    Total,
    Median,
    Percentile,
    PercentileCont,
    PercentileDisc,
    GroupConcat,
    JsonGroupArray,
    JsonbGroupArray,
    JsonGroupObject,
    JsonbGroupObject,
    Min,
    Max,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AggregateArg {
    Wildcard,
    Expr {
        expr: ScalarExpr,
        distinct: bool,
        order_by: Vec<OrderBy>,
    },
    GroupConcat {
        expr: ScalarExpr,
        separator: Option<ScalarExpr>,
        distinct: bool,
        order_by: Vec<OrderBy>,
    },
    JsonGroupObject {
        key: ScalarExpr,
        value: ScalarExpr,
        order_by: Vec<OrderBy>,
    },
    Percentile {
        expr: ScalarExpr,
        fraction: ScalarExpr,
        order_by: Vec<OrderBy>,
    },
}

#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Statement {
    WithDml {
        with: WithClause,
        statement: Box<Statement>,
    },
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
        select: SelectStatement,
        temporary: bool,
    },
    CreateTableAsValues {
        name: String,
        if_not_exists: bool,
        with: Option<WithClause>,
        rows: Vec<Vec<ScalarExpr>>,
        temporary: bool,
    },
    CreateView {
        name: String,
        columns: Option<Vec<String>>,
        if_not_exists: bool,
        select: SelectStatement,
        temporary: bool,
    },
    CreateIndex {
        name: String,
        schema: Option<String>,
        table: String,
        columns: Vec<String>,
        decorated_columns: Option<Vec<String>>,
        unique: bool,
        predicate: Option<String>,
        if_not_exists: bool,
    },
    CreateTrigger {
        name: String,
        schema: Option<String>,
        table: String,
        sql: String,
        if_not_exists: bool,
    },
    DropTable {
        name: String,
        schema: Option<String>,
        if_exists: bool,
    },
    DropView {
        name: String,
        schema: Option<String>,
        if_exists: bool,
    },
    DropIndex {
        name: String,
        schema: Option<String>,
        if_exists: bool,
    },
    DropTrigger {
        name: String,
        schema: Option<String>,
        if_exists: bool,
    },
    AlterTable {
        table: String,
        schema: Option<String>,
        action: AlterTableAction,
    },
    Insert {
        table: String,
        columns: Option<Vec<String>>,
        or_conflict: Option<String>,
        values: Vec<Value>,
    },
    InsertReturning {
        table: String,
        columns: Option<Vec<String>>,
        or_conflict: Option<String>,
        values: Vec<Value>,
        returning: Vec<SelectItem>,
    },
    InsertUpsert {
        table: String,
        columns: Option<Vec<String>>,
        values: Vec<Value>,
        upsert: UpsertClause,
    },
    InsertUpsertReturning {
        table: String,
        columns: Option<Vec<String>>,
        values: Vec<Value>,
        upsert: UpsertClause,
        returning: Vec<SelectItem>,
    },
    InsertMany {
        table: String,
        columns: Option<Vec<String>>,
        or_conflict: Option<String>,
        rows: Vec<Vec<Value>>,
    },
    InsertManyReturning {
        table: String,
        columns: Option<Vec<String>>,
        or_conflict: Option<String>,
        rows: Vec<Vec<Value>>,
        returning: Vec<SelectItem>,
    },
    InsertManyUpsert {
        table: String,
        columns: Option<Vec<String>>,
        rows: Vec<Vec<Value>>,
        upsert: UpsertClause,
    },
    InsertManyUpsertReturning {
        table: String,
        columns: Option<Vec<String>>,
        rows: Vec<Vec<Value>>,
        upsert: UpsertClause,
        returning: Vec<SelectItem>,
    },
    InsertDoNothing {
        table: String,
        columns: Option<Vec<String>>,
        target: Option<Vec<String>>,
        values: Vec<Value>,
    },
    InsertDoNothingReturning {
        table: String,
        columns: Option<Vec<String>>,
        target: Option<Vec<String>>,
        values: Vec<Value>,
        returning: Vec<SelectItem>,
    },
    InsertManyDoNothing {
        table: String,
        columns: Option<Vec<String>>,
        target: Option<Vec<String>>,
        rows: Vec<Vec<Value>>,
    },
    InsertManyDoNothingReturning {
        table: String,
        columns: Option<Vec<String>>,
        target: Option<Vec<String>>,
        rows: Vec<Vec<Value>>,
        returning: Vec<SelectItem>,
    },
    InsertExpr {
        table: String,
        columns: Option<Vec<String>>,
        or_conflict: Option<String>,
        values: Vec<ScalarExpr>,
    },
    InsertExprReturning {
        table: String,
        columns: Option<Vec<String>>,
        or_conflict: Option<String>,
        values: Vec<ScalarExpr>,
        returning: Vec<SelectItem>,
    },
    InsertExprUpsert {
        table: String,
        columns: Option<Vec<String>>,
        values: Vec<ScalarExpr>,
        upsert: UpsertClause,
    },
    InsertExprUpsertReturning {
        table: String,
        columns: Option<Vec<String>>,
        values: Vec<ScalarExpr>,
        upsert: UpsertClause,
        returning: Vec<SelectItem>,
    },
    InsertManyExpr {
        table: String,
        columns: Option<Vec<String>>,
        or_conflict: Option<String>,
        rows: Vec<Vec<ScalarExpr>>,
    },
    InsertManyExprReturning {
        table: String,
        columns: Option<Vec<String>>,
        or_conflict: Option<String>,
        rows: Vec<Vec<ScalarExpr>>,
        returning: Vec<SelectItem>,
    },
    InsertManyExprUpsert {
        table: String,
        columns: Option<Vec<String>>,
        rows: Vec<Vec<ScalarExpr>>,
        upsert: UpsertClause,
    },
    InsertManyExprUpsertReturning {
        table: String,
        columns: Option<Vec<String>>,
        rows: Vec<Vec<ScalarExpr>>,
        upsert: UpsertClause,
        returning: Vec<SelectItem>,
    },
    InsertExprDoNothing {
        table: String,
        columns: Option<Vec<String>>,
        target: Option<Vec<String>>,
        values: Vec<ScalarExpr>,
    },
    InsertExprDoNothingReturning {
        table: String,
        columns: Option<Vec<String>>,
        target: Option<Vec<String>>,
        values: Vec<ScalarExpr>,
        returning: Vec<SelectItem>,
    },
    InsertManyExprDoNothing {
        table: String,
        columns: Option<Vec<String>>,
        target: Option<Vec<String>>,
        rows: Vec<Vec<ScalarExpr>>,
    },
    InsertManyExprDoNothingReturning {
        table: String,
        columns: Option<Vec<String>>,
        target: Option<Vec<String>>,
        rows: Vec<Vec<ScalarExpr>>,
        returning: Vec<SelectItem>,
    },
    InsertSelect {
        table: String,
        columns: Option<Vec<String>>,
        or_conflict: Option<String>,
        select: Box<SelectStatement>,
    },
    InsertSelectReturning {
        table: String,
        columns: Option<Vec<String>>,
        or_conflict: Option<String>,
        select: Box<SelectStatement>,
        returning: Vec<SelectItem>,
    },
    InsertSelectUpsert {
        table: String,
        columns: Option<Vec<String>>,
        select: Box<SelectStatement>,
        upsert: UpsertClause,
    },
    InsertSelectUpsertReturning {
        table: String,
        columns: Option<Vec<String>>,
        select: Box<SelectStatement>,
        upsert: UpsertClause,
        returning: Vec<SelectItem>,
    },
    InsertSelectDoNothing {
        table: String,
        columns: Option<Vec<String>>,
        target: Option<Vec<String>>,
        select: Box<SelectStatement>,
    },
    InsertSelectDoNothingReturning {
        table: String,
        columns: Option<Vec<String>>,
        target: Option<Vec<String>>,
        select: Box<SelectStatement>,
        returning: Vec<SelectItem>,
    },
    Delete {
        table: String,
        schema: Option<String>,
        table_alias: Option<String>,
        index_hint: Option<TableIndexHint>,
        filter: Option<Expr>,
    },
    DeleteLimited {
        table: String,
        schema: Option<String>,
        table_alias: Option<String>,
        index_hint: Option<TableIndexHint>,
        filter: Option<Expr>,
        order_by: Vec<OrderBy>,
        limit: Option<usize>,
        offset: Option<usize>,
    },
    DeleteReturning {
        table: String,
        schema: Option<String>,
        table_alias: Option<String>,
        index_hint: Option<TableIndexHint>,
        filter: Option<Expr>,
        returning: Vec<SelectItem>,
    },
    DeleteReturningLimited {
        table: String,
        schema: Option<String>,
        table_alias: Option<String>,
        index_hint: Option<TableIndexHint>,
        filter: Option<Expr>,
        returning: Vec<SelectItem>,
        order_by: Vec<OrderBy>,
        limit: Option<usize>,
        offset: Option<usize>,
    },
    Update {
        table: String,
        schema: Option<String>,
        table_alias: Option<String>,
        index_hint: Option<TableIndexHint>,
        or_conflict: Option<String>,
        assignments: Vec<Assignment>,
        from: Option<FromItem>,
        filter: Option<Expr>,
    },
    UpdateLimited {
        table: String,
        schema: Option<String>,
        table_alias: Option<String>,
        index_hint: Option<TableIndexHint>,
        or_conflict: Option<String>,
        assignments: Vec<Assignment>,
        from: Option<FromItem>,
        filter: Option<Expr>,
        order_by: Vec<OrderBy>,
        limit: Option<usize>,
        offset: Option<usize>,
    },
    UpdateReturning {
        table: String,
        schema: Option<String>,
        table_alias: Option<String>,
        index_hint: Option<TableIndexHint>,
        or_conflict: Option<String>,
        assignments: Vec<Assignment>,
        from: Option<FromItem>,
        filter: Option<Expr>,
        returning: Vec<SelectItem>,
    },
    UpdateReturningLimited {
        table: String,
        schema: Option<String>,
        table_alias: Option<String>,
        index_hint: Option<TableIndexHint>,
        or_conflict: Option<String>,
        assignments: Vec<Assignment>,
        from: Option<FromItem>,
        filter: Option<Expr>,
        returning: Vec<SelectItem>,
        order_by: Vec<OrderBy>,
        limit: Option<usize>,
        offset: Option<usize>,
    },
    Values(Vec<Vec<ScalarExpr>>),
    ValuesWith {
        with: WithClause,
        rows: Vec<Vec<ScalarExpr>>,
    },
    Select(SelectStatement),
    ExplainQueryPlan(Box<Statement>),
    Analyze,
    Reindex,
    Vacuum,
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
    PragmaOptimize,
    PragmaShrinkMemory,
    PragmaIncrementalVacuum,
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
    Begin {
        isolation_level: Option<IsolationLevel>,
    },
    Commit,
    Rollback,
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TableConstraint {
    Check(CheckConstraint),
    ForeignKey(ForeignKey),
    PrimaryKey(PrimaryKeyConstraint),
    Unique(UniqueConstraint),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SelectItem {
    Wildcard,
    Column(String),
    AliasedColumn {
        name: String,
        alias: String,
    },
    Expr {
        expr: ScalarExpr,
        alias: Option<String>,
    },
    Aggregate {
        func: AggregateFunc,
        arg: AggregateArg,
        filter: Option<Expr>,
        alias: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScalarExpr {
    Literal(Value),
    Column(String),
    Tuple(Vec<ScalarExpr>),
    UnaryPlus(Box<ScalarExpr>),
    UnaryMinus(Box<ScalarExpr>),
    BitNot(Box<ScalarExpr>),
    Not(Box<ScalarExpr>),
    Cast {
        expr: Box<ScalarExpr>,
        ty: ColumnType,
    },
    Collate {
        expr: Box<ScalarExpr>,
        collation: String,
    },
    Is {
        left: Box<ScalarExpr>,
        right: Box<ScalarExpr>,
        negated: bool,
    },
    IsBool {
        expr: Box<ScalarExpr>,
        value: bool,
        negated: bool,
    },
    InList {
        expr: Box<ScalarExpr>,
        values: Vec<ScalarExpr>,
        negated: bool,
    },
    InSubquery {
        expr: Box<ScalarExpr>,
        query: Box<SelectStatement>,
        negated: bool,
    },
    Subquery {
        query: Box<SelectStatement>,
    },
    Like {
        expr: Box<ScalarExpr>,
        pattern: Box<ScalarExpr>,
        escape: Option<Box<ScalarExpr>>,
        negated: bool,
    },
    Glob {
        expr: Box<ScalarExpr>,
        pattern: Box<ScalarExpr>,
        negated: bool,
    },
    Between {
        expr: Box<ScalarExpr>,
        low: Box<ScalarExpr>,
        high: Box<ScalarExpr>,
        negated: bool,
    },
    Compare {
        left: Box<ScalarExpr>,
        op: CompareOp,
        right: Box<ScalarExpr>,
    },
    CompareSubquery {
        left: Box<ScalarExpr>,
        op: CompareOp,
        query: Box<SelectStatement>,
    },
    Case {
        base: Option<Box<ScalarExpr>>,
        when_then_clauses: Vec<(ScalarExpr, ScalarExpr)>,
        else_expr: Option<Box<ScalarExpr>>,
    },
    Binary {
        left: Box<ScalarExpr>,
        op: ScalarBinaryOp,
        right: Box<ScalarExpr>,
    },
    Function {
        func: ScalarFunc,
        args: Vec<ScalarExpr>,
    },
    WindowFunction {
        func: WindowFunc,
        args: Vec<ScalarExpr>,
        partition_by: Vec<ScalarExpr>,
        order_by: Vec<OrderBy>,
        frame: WindowFrame,
        exclude: WindowExclude,
        window_name: Option<String>,
        filter: Option<Box<Expr>>,
    },
    Aggregate {
        func: AggregateFunc,
        arg: Box<AggregateArg>,
        filter: Option<Box<Expr>>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScalarBinaryOp {
    Add,
    Subtract,
    Multiply,
    Divide,
    Modulo,
    BitAnd,
    BitOr,
    ShiftLeft,
    ShiftRight,
    Concat,
    JsonExtract,
    JsonExtractText,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowFunc {
    RowNumber,
    Rank,
    DenseRank,
    Lag,
    Lead,
    Ntile,
    PercentRank,
    CumeDist,
    FirstValue,
    LastValue,
    NthValue,
    Count,
    Sum,
    Avg,
    Total,
    Min,
    Max,
    GroupConcat,
    JsonGroupArray,
    JsonGroupObject,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowFrame {
    Default,
    RangePrecedingToCurrentRow(WindowRangeOffset),
    RangePrecedingToPreceding {
        start: WindowRangeOffset,
        end: WindowRangeOffset,
    },
    RangePrecedingToFollowing {
        preceding: WindowRangeOffset,
        following: WindowRangeOffset,
    },
    RangePrecedingToUnboundedFollowing(WindowRangeOffset),
    RangeUnboundedPrecedingToPreceding(WindowRangeOffset),
    RangeUnboundedPrecedingToFollowing(WindowRangeOffset),
    RangeCurrentRowToFollowing(WindowRangeOffset),
    RangeFollowingToFollowing {
        start: WindowRangeOffset,
        end: WindowRangeOffset,
    },
    RangeFollowingToUnboundedFollowing(WindowRangeOffset),
    GroupsPrecedingToCurrentRow(usize),
    GroupsPrecedingToPreceding {
        start: usize,
        end: usize,
    },
    GroupsPrecedingToFollowing {
        preceding: usize,
        following: usize,
    },
    GroupsPrecedingToUnboundedFollowing(usize),
    GroupsCurrentRow,
    GroupsCurrentRowToFollowing(usize),
    GroupsCurrentRowToUnboundedFollowing,
    GroupsFollowingToFollowing {
        start: usize,
        end: usize,
    },
    GroupsFollowingToUnboundedFollowing(usize),
    GroupsUnboundedPrecedingToCurrentRow,
    GroupsUnboundedPrecedingAndFollowing,
    RowsPrecedingToCurrentRow(usize),
    RowsPrecedingToPreceding {
        start: usize,
        end: usize,
    },
    RowsPrecedingToFollowing {
        preceding: usize,
        following: usize,
    },
    RowsPrecedingToUnboundedFollowing(usize),
    RowsCurrentRow,
    RowsCurrentRowToFollowing(usize),
    RowsCurrentRowToUnboundedFollowing,
    RowsFollowingToFollowing {
        start: usize,
        end: usize,
    },
    RowsFollowingToUnboundedFollowing(usize),
    RowsUnboundedPrecedingToCurrentRow,
    RowsUnboundedPrecedingAndFollowing,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WindowRangeOffset(u64);

impl WindowRangeOffset {
    #[must_use]
    pub fn new(value: f64) -> Self {
        Self(value.to_bits())
    }

    #[must_use]
    pub fn value(self) -> f64 {
        f64::from_bits(self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowExclude {
    NoOthers,
    CurrentRow,
    Group,
    Ties,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScalarFunc {
    Length,
    OctetLength,
    MinScalar,
    MaxScalar,
    Date,
    Time,
    DateTime,
    TimeDiff,
    Strftime,
    JulianDay,
    UnixEpoch,
    Changes,
    TotalChanges,
    Printf,
    IIf,
    If,
    Concat,
    ConcatWs,
    Likely,
    Unlikely,
    SqliteVersion,
    SqliteSourceId,
    SqliteCompileOptionUsed,
    SqliteCompileOptionGet,
    SqliteLog,
    Sign,
    RandomBlob,
    Random,
    Unhex,
    Unistr,
    UnistrQuote,
    Likelihood,
    Mod,
    Ceil,
    Ceiling,
    Floor,
    Trunc,
    Pi,
    Sqrt,
    Power,
    Exp,
    Sin,
    Cos,
    Tan,
    Sinh,
    Cosh,
    Tanh,
    Acos,
    Asin,
    Atan,
    Atan2,
    Acosh,
    Asinh,
    Atanh,
    Ln,
    Log10,
    Log2,
    Log,
    Degrees,
    Radians,
    TypeOf,
    Subtype,
    Hex,
    Substr,
    Instr,
    Replace,
    LikeFunc,
    GlobFunc,
    RegexpFunc,
    MatchFunc,
    Quote,
    Unicode,
    Char,
    ZeroBlob,
    Trim,
    LTrim,
    RTrim,
    Lower,
    Upper,
    Abs,
    Round,
    Coalesce,
    IfNull,
    NullIf,
    Unknown,
    Json,
    Jsonb,
    JsonValid,
    JsonErrorPosition,
    JsonPretty,
    JsonQuote,
    JsonExtract,
    JsonbExtract,
    JsonType,
    JsonArray,
    JsonbArray,
    JsonObject,
    JsonbObject,
    JsonArrayLength,
    JsonRemove,
    JsonbRemove,
    JsonSet,
    JsonbSet,
    JsonInsert,
    JsonbInsert,
    JsonReplace,
    JsonbReplace,
    JsonPatch,
    JsonbPatch,
    LastInsertRowId,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Expr {
    Compare {
        column: String,
        op: CompareOp,
        value: Value,
    },
    CompareColumns {
        left: String,
        op: CompareOp,
        right: String,
    },
    CompareScalar {
        left: ScalarExpr,
        op: CompareOp,
        right: ScalarExpr,
    },
    IsNull {
        column: String,
        negated: bool,
    },
    IsNullScalar {
        expr: ScalarExpr,
        negated: bool,
    },
    Is {
        left: ScalarExpr,
        right: ScalarExpr,
        negated: bool,
    },
    IsBool {
        expr: ScalarExpr,
        value: bool,
        negated: bool,
        explicit: bool,
    },
    InSubquery {
        column: String,
        query: Box<SelectStatement>,
        negated: bool,
    },
    InList {
        column: String,
        values: Vec<Value>,
        negated: bool,
    },
    InSubqueryScalar {
        expr: ScalarExpr,
        query: Box<SelectStatement>,
        negated: bool,
    },
    InListScalar {
        expr: ScalarExpr,
        values: Vec<ScalarExpr>,
        negated: bool,
    },
    CompareSubquery {
        column: String,
        op: CompareOp,
        query: Box<SelectStatement>,
    },
    CompareSubqueryScalar {
        left: ScalarExpr,
        op: CompareOp,
        query: Box<SelectStatement>,
    },
    ExistsSubquery {
        query: Box<SelectStatement>,
        negated: bool,
    },
    Like {
        column: String,
        pattern: Box<ScalarExpr>,
        escape: Option<Box<ScalarExpr>>,
        negated: bool,
    },
    LikeScalar {
        expr: ScalarExpr,
        pattern: Box<ScalarExpr>,
        escape: Option<Box<ScalarExpr>>,
        negated: bool,
    },
    Glob {
        column: String,
        pattern: Box<ScalarExpr>,
        negated: bool,
    },
    GlobScalar {
        expr: ScalarExpr,
        pattern: Box<ScalarExpr>,
        negated: bool,
    },
    Between {
        column: String,
        low: Value,
        high: Value,
        negated: bool,
    },
    BetweenScalar {
        expr: ScalarExpr,
        low: ScalarExpr,
        high: ScalarExpr,
        negated: bool,
    },
    Not(Box<Expr>),
    And(Box<Expr>, Box<Expr>),
    Or(Box<Expr>, Box<Expr>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompareOp {
    Eq,
    Ne,
    Gt,
    Gte,
    Lt,
    Lte,
}

#[cfg(test)]
mod tests {
    use crate::common::types::{ColumnDef, ColumnType, Value};

    use super::{
        AggregateArg, AggregateFunc, CompareOp, Expr, FromItem, SelectItem, SelectStatement,
        Statement,
    };

    #[test]
    fn statement_variants_preserve_payloads() {
        let statement = Statement::CreateTable {
            name: "users".to_string(),
            columns: vec![ColumnDef::primary_key("id", ColumnType::Integer)],
            constraints: vec![],
            strict: false,
            without_rowid: false,
            if_not_exists: false,
            temporary: false,
        };
        assert_eq!(
            statement,
            Statement::CreateTable {
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
    fn select_items_and_exprs_are_comparable() {
        assert_eq!(
            SelectItem::Column("name".to_string()),
            SelectItem::Column("name".to_string())
        );
        assert_eq!(
            Expr::Compare {
                column: "id".to_string(),
                op: CompareOp::Eq,
                value: Value::Integer(1),
            },
            Expr::Compare {
                column: "id".to_string(),
                op: CompareOp::Eq,
                value: Value::Integer(1),
            }
        );
        assert_ne!(
            Expr::Compare {
                column: "id".to_string(),
                op: CompareOp::Gt,
                value: Value::Integer(1),
            },
            Expr::Compare {
                column: "id".to_string(),
                op: CompareOp::Lt,
                value: Value::Integer(1),
            }
        );
        assert_eq!(
            Expr::Not(Box::new(Expr::IsNull {
                column: "name".to_string(),
                negated: false,
            })),
            Expr::Not(Box::new(Expr::IsNull {
                column: "name".to_string(),
                negated: false,
            }))
        );
        assert_eq!(
            SelectItem::Aggregate {
                func: AggregateFunc::Count,
                arg: AggregateArg::Wildcard,
                filter: None,
                alias: Some("total".to_string()),
            },
            SelectItem::Aggregate {
                func: AggregateFunc::Count,
                arg: AggregateArg::Wildcard,
                filter: None,
                alias: Some("total".to_string()),
            }
        );
        assert_eq!(
            Statement::Select(SelectStatement {
                with: None,
                distinct: false,
                columns: vec![SelectItem::Column("id".to_string())],
                from: FromItem::Table {
                    name: "users".to_string(),
                    schema: None,
                    alias: None,
                },
                joins: vec![],
                filter: None,
                group_by: vec![],
                having: None,
                compounds: vec![],
                order_by: vec![],
                limit: None,
                offset: None,
            }),
            Statement::Select(SelectStatement {
                with: None,
                distinct: false,
                columns: vec![SelectItem::Column("id".to_string())],
                from: FromItem::Table {
                    name: "users".to_string(),
                    schema: None,
                    alias: None,
                },
                joins: vec![],
                filter: None,
                group_by: vec![],
                having: None,
                compounds: vec![],
                order_by: vec![],
                limit: None,
                offset: None,
            })
        );
    }
}
