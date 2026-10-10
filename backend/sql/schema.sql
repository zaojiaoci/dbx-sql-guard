-- DBX SQL Guard 的本地存储。文件位于插件自己的 DBX_PLUGIN_DATA_DIR 下，
-- 绝不会写进 DBX 的主库。全部语句带 IF NOT EXISTS：插件升级时会重复执行，
-- 必须对已有库是无害的（也因此这里只能加表/加索引，不能改已有列）。

-- 键值表。目前只存一行：key = 'rules'，value = 整个 RuleSet 的 JSON。
-- 用一张通用表而不是给每个规则字段建列，是为了让规则集可以自由扩展而
-- 不需要写数据迁移；缺字段时由 serde 的 #[serde(default)] 兜底。
CREATE TABLE IF NOT EXISTS meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

-- 审计记录。一行 = 一次 SQL 提交，写入后内容字段不再修改。
--
-- 注意这里没有显式声明主键自增列：SQLite 会为普通表隐式维护 rowid，
-- 而哈希链正是按 rowid 顺序串起来的（store.rs 的 submit 取
-- `ORDER BY rowid DESC LIMIT 1` 当上一环，verify 按 `rowid ASC` 重算）。
-- 所以这张表不能改成 WITHOUT ROWID，否则链的"顺序"就没有依据了。
CREATE TABLE IF NOT EXISTS audit_record (
    record_id      TEXT PRIMARY KEY,   -- sg_ 开头的短 ID，见 util::short_id
    sql            TEXT NOT NULL,      -- 脱敏后的 SQL：字面量和注释内容已替换成 ?
    sql_hash       TEXT NOT NULL,      -- 原始 SQL 规范化后的 SHA-256，可用于比对是否同一条语句
    connection_id  TEXT,               -- 以下三项由调用方提供，可能为空。工作台已经不再
    database_name  TEXT,               -- 收集 connection_id / db_type：宿主只在插件声明了
    db_type        TEXT,               -- connection-provider 时才下发连接信息，而声明它就
                                       -- 意味着持有数据库凭据。两列保留是为了让老记录仍能
                                       -- 读出原值，新记录一律为 NULL。
    operator       TEXT NOT NULL,      -- 提交人（调用方自报，缺省退回操作系统用户名）
    risk_level     TEXT NOT NULL,      -- low / medium / high
    status         TEXT NOT NULL,      -- pending / autoApproved / approved / rejected
    findings       TEXT NOT NULL,      -- Finding 数组的 JSON 文本，参与 digest
    estimated_rows INTEGER,            -- 调用方自报的预计影响行数
    approver       TEXT,               -- 以下三项在审批时才填
    note           TEXT,
    created_at_ms  INTEGER NOT NULL,
    decided_at_ms  INTEGER,
    prev_digest    TEXT NOT NULL,      -- 上一行的 digest，第一行是全零的 GENESIS
    digest         TEXT NOT NULL,      -- SHA-256(prev_digest ‖ 上面所有不可变字段)
    decision_digest TEXT NOT NULL      -- SHA-256(record_id ‖ status ‖ approver ‖ note ‖ decided_at_ms)
);

-- 列表默认按时间倒序，所以 created_at_ms 的降序索引是主查询路径。
-- status 和 risk_level 各自建索引，对应 UI 上最常用的两个筛选器。
CREATE INDEX IF NOT EXISTS idx_audit_created ON audit_record (created_at_ms DESC);
CREATE INDEX IF NOT EXISTS idx_audit_status  ON audit_record (status);
CREATE INDEX IF NOT EXISTS idx_audit_risk    ON audit_record (risk_level);
