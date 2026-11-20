//! Import ZCode's per-request usage under `app_type = "zcode"`.
//!
//! Primary source: `<ZCODE_HOME|~/.zcode>/cli/db/db.sqlite`, table
//! `model_usage` — one row per model request, ZCode's permanent authoritative
//! history. `input_tokens` is cache-inclusive there (same OpenAI semantics as
//! the rollout `inputTokens`), so rows are stored as-is with
//! `input_token_semantics = TOTAL` and cost calc plus aggregation
//! normalization go through `CACHE_INCLUSIVE_APP_TYPES`. `model_usage.id` is
//! the table's primary key, making `zcode:v2:{id}` receipts idempotent;
//! incrementality is a `started_at` watermark (`zcode:v2:{db path}` cursor)
//! rewound by a lookback window because rows land only when a request
//! finishes. error/cancelled rows carry no tokens (audited: 181 + 377 rows,
//! all zero), so all-zero rows are skipped regardless of status.
//!
//! Fallback source: `cli/rollout/model-io-sess_<sessionId>.jsonl` (v1
//! importer). Rollout is minute-scale scratch that ZCode deletes when a
//! session ends, so an importer tied to it only sees whatever happened to be
//! on disk during a sync window (audited coverage of the db total: 0.0346%);
//! it now runs only when db.sqlite cannot be opened read-only. Each rollout
//! line is one complete request with usage at `/response/usage`
//! (`inputTokens/outputTokens/cacheReadTokens/cacheWriteTokens`);
//! `requestId` and `attempt` (a retry counter, observed values include 11)
//! form the request id, so retried lines import as separate rows. Lines can
//! reach ~300KB (full request/response bodies), so files are read
//! incrementally with `read_until`; a trailing partial line waits for the
//! next round. The daily logs under `cli/log/` redact usage to `[Redacted]`
//! and are not usable.
//!
//! The two sources have no request-level join key (audited: 0/81 rollout
//! requestIds match `model_usage.logical_request_id`), so they must never
//! coexist: whenever the rollout namespaces (`zcode_session` receipts,
//! `zcode:v1:` cursors) are present, the db path first purges every derived
//! zcode row — logs, receipts, cursors and rollups, all rebuilt from this
//! importer's data alone — and then re-imports the full db history.
//!
//! ZCode's own files (db.sqlite, rollout) are only ever opened read-only.

use crate::config::get_home_dir;
use crate::database::{lock_conn, Database};
use crate::error::AppError;
use crate::proxy::usage::calculator::ModelPricing;
use crate::proxy::usage::parser::TokenUsage;
use crate::services::{
    session_usage::{
        cost_columns, load_sync_cursors, metadata_modified_nanos, update_sync_state_on_conn,
        SessionSyncResult,
    },
    sql_helpers::INPUT_TOKEN_SEMANTICS_TOTAL,
    usage_stats::find_model_pricing,
};
use crate::session_manager::providers::utils::parse_timestamp_to_ms;
use rust_decimal::Decimal;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    fs,
    io::{BufRead, BufReader, Seek, SeekFrom},
    path::{Path, PathBuf},
    time::Duration,
};

/// Receipt/data-source namespace of the rollout fallback (v1 importer).
const DATA_SOURCE: &str = "zcode_session";
/// Receipt/data-source namespace of the db.sqlite importer (v2).
const DB_DATA_SOURCE: &str = "zcode_db";
/// Rollout cursor key prefix. Bump when "which records count as a request"
/// changes; the permanent receipts in `session_usage_dedup` keep the forced
/// full re-read from double-counting already-imported requests.
const CURSOR_PREFIX: &str = "zcode:v1:";
/// Watermark cursor key prefix of the db.sqlite importer.
const DB_CURSOR_PREFIX: &str = "zcode:v2:";
/// `model_usage` rows are inserted when a request finishes, so a `started_at`
/// watermark rewinds by this window to catch late commits; receipts make the
/// re-scan idempotent.
const WATERMARK_LOOKBACK_MS: i64 = 60 * 60 * 1000;
/// Detail inserts commit (with the watermark advance) in chunks of this size,
/// keeping the cc-switch connection lock short during the first full import
/// (~144k rows on the audited database).
const IMPORT_CHUNK: usize = 2_000;
/// Grace period for a busy ZCode writer; WAL reads normally never block.
const DB_BUSY_TIMEOUT: Duration = Duration::from_secs(5);

pub fn sync_zcode_usage(db: &Database) -> Result<SessionSyncResult, AppError> {
    let home = std::env::var_os("ZCODE_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| get_home_dir().join(".zcode"));
    sync_from_home(db, &home)
}

fn sync_from_home(db: &Database, home: &Path) -> Result<SessionSyncResult, AppError> {
    let db_path = home.join("cli").join("db").join("db.sqlite");
    match open_zcode_db(&db_path) {
        Some(source) => sync_model_usage(db, &source, &db_path),
        // db.sqlite unavailable (missing, not a ZCode database, or the read
        // cannot ride the WAL): keep the v1 rollout importer as fallback so a
        // temporarily unreadable database does not silently drop usage.
        None => sync_rollout(db, &home.join("cli").join("rollout")),
    }
}

/// Open ZCode's history database read-only. `None` (rather than an error)
/// means "source unavailable" and routes the caller to the rollout fallback.
fn open_zcode_db(path: &Path) -> Option<rusqlite::Connection> {
    if !path.is_file() {
        return None;
    }
    let conn =
        rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .ok()?;
    // WAL reads need the -shm file a live writer maintains; a brief grace
    // period avoids failing on momentary exclusive locks.
    conn.busy_timeout(DB_BUSY_TIMEOUT).ok()?;
    sqlite_table_exists(&conn, "model_usage")
        .ok()?
        .then_some(conn)
}

fn sqlite_table_exists(conn: &rusqlite::Connection, table: &str) -> rusqlite::Result<bool> {
    conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1)",
        [table],
        |row| row.get(0),
    )
}

struct Entry {
    request_id: String,
    session_id: String,
    model: String,
    usage: TokenUsage,
    created_ms: i64,
    latency_ms: i64,
}

/// Insert one zcode usage row behind its receipt. Returns false when the
/// receipt already exists (already imported). `data_source` namespaces the
/// receipt (rollout fallback vs db.sqlite) and lands in both the
/// `provider_type` and `data_source` columns, matching the v1 layout.
fn insert_entry_on_conn(
    tx: &rusqlite::Connection,
    data_source: &str,
    entry: &Entry,
    pricing: Option<&ModelPricing>,
) -> Result<bool, AppError> {
    if tx.execute(
        "INSERT OR IGNORE INTO session_usage_dedup (data_source, request_id, semantic_id, has_entry_id) VALUES (?1, ?2, ?2, 1)",
        [data_source, &entry.request_id],
    )? == 0
    {
        return Ok(false);
    }
    let usage = &entry.usage;
    let amounts = cost_columns("zcode", usage, pricing, Decimal::ONE);
    tx.execute(
        "INSERT OR IGNORE INTO proxy_request_logs (
            request_id, provider_id, app_type, model, request_model, input_tokens, output_tokens, cache_read_tokens, cache_creation_tokens,
            input_cost_usd, output_cost_usd, cache_read_cost_usd, cache_creation_cost_usd, total_cost_usd,
            latency_ms, status_code, session_id, provider_type, is_streaming, cost_multiplier, created_at, data_source, input_token_semantics
        ) VALUES (?1, '_zcode_session', 'zcode', ?2, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, 200, ?13, ?14, 1, '1', ?15, ?14, ?16)",
        rusqlite::params![
            entry.request_id,
            entry.model,
            usage.input_tokens,
            usage.output_tokens,
            usage.cache_read_tokens,
            usage.cache_creation_tokens,
            amounts[0],
            amounts[1],
            amounts[2],
            amounts[3],
            amounts[4],
            entry.latency_ms,
            entry.session_id,
            data_source,
            entry.created_ms / 1000,
            INPUT_TOKEN_SEMANTICS_TOTAL,
        ],
    )?;
    Ok(true)
}

/// True when rollout-era (v1) derived data is present: `zcode_session`
/// receipts or `zcode:v1:` cursors. Written by every fallback run, so this
/// also detects "the fallback ran since the last db import".
fn legacy_rollout_data_exists(db: &Database) -> Result<bool, AppError> {
    let conn = lock_conn!(db.conn);
    conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM session_usage_dedup WHERE data_source = ?1)
            OR EXISTS(SELECT 1 FROM session_log_sync WHERE file_path LIKE ?2)",
        [DATA_SOURCE, &format!("{CURSOR_PREFIX}%")],
        |row| row.get(0),
    )
    .map_err(|e| AppError::Database(format!("检测 ZCode 旧导入残留失败: {e}")))
}

/// Drop every derived zcode row so the db.sqlite import rebuilds history from
/// scratch: the two sources have no request-level join key, so keeping any of
/// these would double-count. `usage_daily_rollups` only ever held aggregates
/// of the very rows deleted here (zcode has no proxy handler), so wiping it
/// keeps the re-imported rows from adding into existing rollups; the next
/// rollup rebuilds them from the re-imported details.
fn purge_zcode_rows_on_conn(conn: &rusqlite::Connection) -> Result<(), AppError> {
    for sql in [
        "DELETE FROM proxy_request_logs WHERE app_type = 'zcode'",
        &format!(
            "DELETE FROM session_usage_dedup WHERE data_source IN ('{DATA_SOURCE}', '{DB_DATA_SOURCE}')"
        ),
        &format!(
            "DELETE FROM session_log_sync WHERE file_path LIKE '{CURSOR_PREFIX}%' OR file_path LIKE '{DB_CURSOR_PREFIX}%'"
        ),
        "DELETE FROM usage_daily_rollups WHERE app_type = 'zcode'",
    ] {
        conn.execute(sql, [])
            .map_err(|e| AppError::Database(format!("清理 ZCode 旧导入失败: {e}")))?;
    }
    Ok(())
}

fn sync_model_usage(
    db: &Database,
    source: &rusqlite::Connection,
    db_path: &Path,
) -> Result<SessionSyncResult, AppError> {
    let watermark_key = format!("{DB_CURSOR_PREFIX}{}", db_path.display());
    let mut result = SessionSyncResult {
        files_scanned: 1,
        ..Default::default()
    };

    // One-time migration / self-heal (see module doc). The purge also removes
    // the v2 watermark cursor, so a full re-import follows; cursors are loaded
    // only afterwards for that reason.
    if legacy_rollout_data_exists(db)? {
        let mut conn = lock_conn!(db.conn);
        let tx = conn.transaction()?;
        purge_zcode_rows_on_conn(&tx)?;
        tx.commit()?;
        log::info!("[ZCODE-SYNC] purged rollout-era import, re-importing from db.sqlite");
    }

    let watermark_ms = load_sync_cursors(db)?
        .get(&watermark_key)
        .map_or(0, |cursor| cursor.last_modified);
    let floor_ms = watermark_ms.saturating_sub(WATERMARK_LOOKBACK_MS);
    let entries = fetch_model_usage_entries(source, floor_ms)?;

    // Resolve pricing once: the model set is tiny (4 distinct on the audited
    // database) and chunk transactions must not re-query it per chunk.
    let mut pricing: HashMap<String, Option<ModelPricing>> = HashMap::new();
    {
        let conn = lock_conn!(db.conn);
        for entry in &entries {
            pricing
                .entry(entry.model.clone())
                .or_insert_with(|| find_model_pricing(&conn, &entry.model));
        }
    }

    let mut watermark_ms = watermark_ms;
    for chunk in entries.chunks(IMPORT_CHUNK) {
        let mut conn = lock_conn!(db.conn);
        let tx = conn.transaction()?;
        for entry in chunk {
            if insert_entry_on_conn(
                &tx,
                DB_DATA_SOURCE,
                entry,
                pricing.get(&entry.model).and_then(Option::as_ref),
            )? {
                result.imported += 1;
            } else {
                result.skipped += 1;
            }
        }
        // Entries are ordered by started_at; advance the watermark with every
        // chunk so a crash resumes mid-history instead of restarting.
        if let Some(last) = chunk.last() {
            watermark_ms = watermark_ms.max(last.created_ms);
        }
        update_sync_state_on_conn(&tx, &watermark_key, watermark_ms, 0)?;
        tx.commit()?;
    }
    Ok(result)
}

/// Rows of `model_usage` with `started_at > floor_ms`, ordered by
/// `(started_at, id)`, skipping rows that billed nothing.
fn fetch_model_usage_entries(
    source: &rusqlite::Connection,
    floor_ms: i64,
) -> Result<Vec<Entry>, AppError> {
    let mut stmt = source
        .prepare(
            "SELECT id, session_id, model_id, started_at, duration_ms, input_tokens,
                    output_tokens, cache_creation_input_tokens, cache_read_input_tokens
             FROM model_usage WHERE started_at > ?1 ORDER BY started_at, id",
        )
        .map_err(|e| AppError::Database(format!("准备 ZCode model_usage 查询失败: {e}")))?;
    let rows = stmt
        .query_map([floor_ms], |row| {
            Ok((
                row.get::<_, Option<String>>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, Option<i64>>(4)?,
                row.get::<_, Option<i64>>(5)?,
                row.get::<_, Option<i64>>(6)?,
                row.get::<_, Option<i64>>(7)?,
                row.get::<_, Option<i64>>(8)?,
            ))
        })
        .map_err(|e| AppError::Database(format!("查询 ZCode model_usage 失败: {e}")))?;

    let mut entries = Vec::new();
    for row in rows {
        let (
            id,
            session_id,
            model_id,
            started_ms,
            duration_ms,
            input,
            output,
            cache_creation,
            cache_read,
        ) = row.map_err(|e| AppError::Database(format!("读取 ZCode model_usage 行失败: {e}")))?;
        if let Some(entry) = db_entry(
            id,
            session_id,
            model_id,
            started_ms,
            duration_ms,
            input,
            output,
            cache_creation,
            cache_read,
        ) {
            entries.push(entry);
        }
    }
    Ok(entries)
}

fn clamp_tokens(value: i64) -> u32 {
    value.clamp(0, u32::MAX as i64) as u32
}

/// One `model_usage` row → one import candidate. Rows without an id are
/// unusable (the id is the idempotency key); rows whose four token fields are
/// all zero billed nothing — error/cancelled rows are all zero in practice,
/// matching the rollout importer's all-zero skip.
#[allow(clippy::too_many_arguments)]
fn db_entry(
    id: Option<String>,
    session_id: Option<String>,
    model_id: Option<String>,
    started_ms: i64,
    duration_ms: Option<i64>,
    input_tokens: Option<i64>,
    output_tokens: Option<i64>,
    cache_creation_tokens: Option<i64>,
    cache_read_tokens: Option<i64>,
) -> Option<Entry> {
    let usage = TokenUsage {
        input_tokens: clamp_tokens(input_tokens.unwrap_or(0)),
        output_tokens: clamp_tokens(output_tokens.unwrap_or(0)),
        cache_read_tokens: clamp_tokens(cache_read_tokens.unwrap_or(0)),
        cache_creation_tokens: clamp_tokens(cache_creation_tokens.unwrap_or(0)),
        cache_creation_1h_tokens: 0,
        model: None,
        message_id: None,
    };
    if usage.input_tokens == 0
        && usage.output_tokens == 0
        && usage.cache_read_tokens == 0
        && usage.cache_creation_tokens == 0
    {
        return None;
    }
    let id = id?;
    let model = model_id.unwrap_or_else(|| "unknown".to_string());
    Some(Entry {
        request_id: format!("zcode:v2:{id}"),
        session_id: session_id.unwrap_or_else(|| "unknown".to_string()),
        model: model.clone(),
        usage: TokenUsage {
            model: Some(model),
            ..usage
        },
        created_ms: started_ms.max(0),
        latency_ms: duration_ms.unwrap_or(0).clamp(0, 3_600_000),
    })
}

fn sync_rollout(db: &Database, root: &Path) -> Result<SessionSyncResult, AppError> {
    let mut result = SessionSyncResult::default();
    let Ok(entries) = fs::read_dir(root) else {
        return Ok(result);
    };
    let mut files: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.is_file()
                && path.extension().and_then(|ext| ext.to_str()) == Some("jsonl")
                && path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("model-io-sess"))
        })
        .collect();
    files.sort();
    if files.is_empty() {
        return Ok(result);
    }
    let cursors = load_sync_cursors(db)?;
    for path in files {
        let Ok(meta) = fs::metadata(&path) else {
            continue;
        };
        let modified = metadata_modified_nanos(&meta);
        let key = format!("{CURSOR_PREFIX}{}", path.to_string_lossy());
        let cursor = cursors.get(&key);
        // Skip only when the file is untouched AND fully consumed: a trailing
        // partial line (size > bytes read) keeps the file eligible next round.
        if cursor
            .is_some_and(|c| c.last_modified == modified && c.last_line_offset == meta.len() as i64)
        {
            continue;
        }
        let offset = cursor
            .map(|c| c.last_line_offset)
            .filter(|offset| *offset >= 0 && *offset <= meta.len() as i64)
            .unwrap_or(0);
        match sync_file(db, &path, &key, modified, offset) {
            Ok(part) => result.merge(part),
            Err(error) => result
                .errors
                .push(format!("ZCode session {}: {error}", path.display())),
        }
    }
    Ok(result)
}

/// One committed line → one import candidate. `line` is the raw record bytes,
/// hashed when `requestId` is missing.
fn entry(record: &Value, line: &[u8]) -> Option<Entry> {
    let usage = record.pointer("/response/usage")?;
    let tokens = |field: &str| {
        usage
            .get(field)
            .and_then(Value::as_u64)
            .unwrap_or(0)
            .min(u32::MAX as u64) as u32
    };
    let usage = TokenUsage {
        input_tokens: tokens("inputTokens"),
        output_tokens: tokens("outputTokens"),
        cache_read_tokens: tokens("cacheReadTokens"),
        cache_creation_tokens: tokens("cacheWriteTokens"),
        cache_creation_1h_tokens: 0,
        model: None,
        message_id: None,
    };
    // Interrupted requests commit `"usage": {}` (sample-3 shape); nothing was billed.
    if usage.input_tokens == 0
        && usage.output_tokens == 0
        && usage.cache_read_tokens == 0
        && usage.cache_creation_tokens == 0
    {
        return None;
    }
    let session_id = record
        .get("sessionId")
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_string();
    let identity = record
        .get("requestId")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| format!("{:x}", Sha256::digest(line)));
    let attempt = record.get("attempt").and_then(Value::as_i64).unwrap_or(0);
    let model = record
        .pointer("/model/modelId")
        .and_then(Value::as_str)
        .or_else(|| {
            record
                .pointer("/request/body/model")
                .and_then(Value::as_str)
        })
        .unwrap_or("unknown")
        .to_string();
    let created_ms = record
        .get("startedAt")
        .and_then(parse_timestamp_to_ms)
        .or_else(|| record.get("completedAt").and_then(parse_timestamp_to_ms))?;
    let latency_ms = record
        .get("durationMs")
        .and_then(Value::as_i64)
        .unwrap_or(0)
        .clamp(0, 3_600_000);
    Some(Entry {
        request_id: format!("zcode:{session_id}:{identity}:{attempt}"),
        session_id,
        usage: TokenUsage {
            model: Some(model.clone()),
            ..usage
        },
        model,
        created_ms,
        latency_ms,
    })
}

fn sync_file(
    db: &Database,
    path: &Path,
    key: &str,
    modified: i64,
    offset: i64,
) -> Result<SessionSyncResult, AppError> {
    let mut file = fs::File::open(path).map_err(|error| AppError::io(path, error))?;
    file.seek(SeekFrom::Start(offset as u64))
        .map_err(|error| AppError::io(path, error))?;
    let mut reader = BufReader::new(file);
    let mut buffer = Vec::new();
    let mut bytes = offset;
    let mut entries = Vec::new();
    loop {
        buffer.clear();
        let size = reader
            .read_until(b'\n', &mut buffer)
            .map_err(|error| AppError::io(path, error))?;
        // The writer may have flushed only part of the last record; retry next round.
        if size == 0 || buffer.last() != Some(&b'\n') {
            break;
        }
        bytes += size as i64;
        if let Some(entry) = serde_json::from_slice::<Value>(&buffer)
            .ok()
            .as_ref()
            .and_then(|record| entry(record, &buffer))
        {
            entries.push(entry);
        }
    }

    let mut result = SessionSyncResult {
        files_scanned: 1,
        ..Default::default()
    };
    let mut conn = lock_conn!(db.conn);
    let tx = conn.transaction()?;
    for entry in entries {
        if insert_entry_on_conn(
            &tx,
            DATA_SOURCE,
            &entry,
            find_model_pricing(&tx, &entry.model).as_ref(),
        )? {
            result.imported += 1;
        } else {
            result.skipped += 1;
        }
    }
    update_sync_state_on_conn(&tx, key, modified, bytes)?;
    tx.commit()?;
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::session_usage::update_sync_state;
    use std::io::Write;

    const SESSION_ID: &str = "sess_335a06d7-c664-47b7-8d3d-073596fc7a9d";
    /// 2026-11-20T15:39:15Z, inside the audited history window.
    const T0_MS: i64 = 1_763_655_555_000;

    /// Fixture mirroring TEMP/zcode-samples/sample-1.jsonl (trimmed).
    fn model_io(request_id: &str, attempt: i64, usage: Value) -> Value {
        serde_json::json!({
            "type": "model_io",
            "sessionId": SESSION_ID,
            "turnId": "turn_3b0a2945-b2df-4688-81ef-445d9a411657",
            "querySource": "main_turn",
            "requestId": request_id,
            "attempt": attempt,
            "traceId": "483fbbb7-4864-4f69-b2a7-cb20454b229f",
            "startedAt": "2026-11-20T14:59:15.079Z",
            "completedAt": "2026-11-20T15:00:31.388Z",
            "durationMs": 76309,
            "model": {"modelId": "GLM-5.3", "providerId": "account:bigmodel-individual-coding-plan"},
            "request": {"body": {"model": "GLM-5.3", "max_tokens": 128000, "stream": true}},
            "response": {"finishReason": "tool-calls", "responseId": "msg_1", "usage": usage},
        })
    }

    fn usage(input: u64, output: u64, cache_read: u64, cache_write: u64) -> Value {
        serde_json::json!({
            "inputTokens": input,
            "outputTokens": output,
            "totalTokens": input + output,
            "cacheReadTokens": cache_read,
            "cacheWriteTokens": cache_write,
        })
    }

    fn session_file(root: &Path) -> PathBuf {
        root.join("model-io-sess_335a06d7-c664-47b7-8d3d-073596fc7a9d.jsonl")
    }

    fn write_lines(path: &Path, records: &[Value]) {
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .unwrap();
        for record in records {
            writeln!(file, "{record}").unwrap();
        }
    }

    /// (request_id, model, session_id, input, output, cache_read, cache_write,
    ///  latency_ms, created_at, provider_id, data_source, input_token_semantics)
    type UsageRow = (
        String,
        String,
        String,
        i64,
        i64,
        i64,
        i64,
        i64,
        i64,
        String,
        String,
        i64,
    );

    fn rows(db: &Database) -> Vec<UsageRow> {
        let conn = db.conn.lock().unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT request_id, model, session_id, input_tokens, output_tokens,
                        cache_read_tokens, cache_creation_tokens, latency_ms, created_at,
                        provider_id, data_source, input_token_semantics
                 FROM proxy_request_logs WHERE app_type = 'zcode' ORDER BY request_id",
            )
            .unwrap();
        stmt.query_map([], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
                row.get(6)?,
                row.get(7)?,
                row.get(8)?,
                row.get(9)?,
                row.get(10)?,
                row.get(11)?,
            ))
        })
        .unwrap()
        .map(Result::unwrap)
        .collect()
    }

    fn epoch_seconds(iso: &str) -> i64 {
        chrono::DateTime::parse_from_rfc3339(iso)
            .unwrap()
            .timestamp()
    }

    /// Create `<home>/cli/db/db.sqlite` with the `model_usage` layout from the
    /// audit report's ER diagram.
    fn zcode_source_db(home: &Path) -> rusqlite::Connection {
        let dir = home.join("cli").join("db");
        fs::create_dir_all(&dir).unwrap();
        let conn = rusqlite::Connection::open(dir.join("db.sqlite")).unwrap();
        conn.execute_batch(
            "CREATE TABLE model_usage (
                id TEXT PRIMARY KEY,
                logical_request_id TEXT,
                attempt_index INTEGER NOT NULL DEFAULT 0,
                session_id TEXT,
                assistant_message_id TEXT,
                query_source TEXT,
                model_id TEXT,
                status TEXT,
                started_at INTEGER,
                duration_ms INTEGER,
                input_tokens INTEGER NOT NULL DEFAULT 0,
                output_tokens INTEGER NOT NULL DEFAULT 0,
                cache_creation_input_tokens INTEGER NOT NULL DEFAULT 0,
                cache_read_input_tokens INTEGER NOT NULL DEFAULT 0,
                raw_usage_json TEXT
            );",
        )
        .unwrap();
        conn
    }

    #[allow(clippy::too_many_arguments)]
    fn insert_usage(
        conn: &rusqlite::Connection,
        id: &str,
        status: &str,
        started_ms: i64,
        duration_ms: i64,
        input: i64,
        output: i64,
        cache_creation: i64,
        cache_read: i64,
    ) {
        conn.execute(
            "INSERT INTO model_usage (id, session_id, model_id, status, started_at, duration_ms,
                input_tokens, output_tokens, cache_creation_input_tokens, cache_read_input_tokens)
             VALUES (?1, ?2, 'GLM-5.3', ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            rusqlite::params![
                id,
                SESSION_ID,
                status,
                started_ms,
                duration_ms,
                input,
                output,
                cache_creation,
                cache_read
            ],
        )
        .unwrap();
    }

    fn scalar(db: &Database, sql: &str) -> i64 {
        let conn = db.conn.lock().unwrap();
        conn.query_row(sql, [], |row| row.get(0)).unwrap()
    }

    #[test]
    fn zcode_usage_imports_model_io_records() {
        let dir = tempfile::tempdir().unwrap();
        // A stray non-model-io file in the same directory must be ignored.
        fs::write(
            dir.path().join("other.jsonl"),
            format!("{}\n", model_io("stray", 1, usage(1, 1, 0, 0))),
        )
        .unwrap();
        write_lines(
            &session_file(dir.path()),
            &[model_io("req-1", 1, usage(121664, 4142, 16832, 0))],
        );
        let db = Database::memory().unwrap();
        let result = sync_rollout(&db, dir.path()).unwrap();
        assert_eq!(
            (result.imported, result.skipped, result.files_scanned),
            (1, 0, 1)
        );
        assert_eq!(
            rows(&db),
            vec![(
                "zcode:sess_335a06d7-c664-47b7-8d3d-073596fc7a9d:req-1:1".into(),
                "GLM-5.3".into(),
                SESSION_ID.into(),
                121664,
                4142,
                16832,
                0,
                76309,
                epoch_seconds("2026-11-20T14:59:15.079Z"),
                "_zcode_session".into(),
                "zcode_session".into(),
                INPUT_TOKEN_SEMANTICS_TOTAL,
            )]
        );
    }

    #[test]
    fn zcode_usage_imports_appended_records_only() {
        let dir = tempfile::tempdir().unwrap();
        let path = session_file(dir.path());
        write_lines(&path, &[model_io("req-1", 1, usage(1000, 10, 100, 0))]);
        let db = Database::memory().unwrap();
        assert_eq!(sync_rollout(&db, dir.path()).unwrap().imported, 1);
        // Unchanged file: the cursor gate skips it entirely.
        assert_eq!(sync_rollout(&db, dir.path()).unwrap().imported, 0);

        let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
        writeln!(file, "{}", model_io("req-2", 1, usage(2000, 20, 200, 0))).unwrap();
        write!(file, "{{\"type\":\"model_io\",\"req").unwrap();
        drop(file);
        assert_eq!(sync_rollout(&db, dir.path()).unwrap().imported, 1);
        assert_eq!(rows(&db).len(), 2);
    }

    #[test]
    fn zcode_usage_retries_partial_tail_next_round() {
        let dir = tempfile::tempdir().unwrap();
        let path = session_file(dir.path());
        write_lines(&path, &[model_io("req-1", 1, usage(1000, 10, 100, 0))]);
        // The writer flushes a prefix of the next record (no trailing newline).
        let full = model_io("req-2", 1, usage(3000, 30, 300, 50)).to_string();
        let (head, tail) = full.split_at(12);
        assert_eq!(head, "{\"type\":\"mod");
        let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
        write!(file, "{head}").unwrap();
        drop(file);
        let db = Database::memory().unwrap();
        assert_eq!(sync_rollout(&db, dir.path()).unwrap().imported, 1);
        assert_eq!(rows(&db).len(), 1);

        // The record completes: the cursor never passed it, so it imports now.
        let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
        writeln!(file, "{tail}").unwrap();
        drop(file);
        assert_eq!(sync_rollout(&db, dir.path()).unwrap().imported, 1);
        let imported = rows(&db);
        assert_eq!(imported.len(), 2);
        assert_eq!(imported[1].3, 3000);
    }

    #[test]
    fn zcode_usage_receipts_prevent_double_import_on_rescan() {
        let dir = tempfile::tempdir().unwrap();
        let path = session_file(dir.path());
        let record = model_io("req-1", 1, usage(1000, 10, 100, 0));
        write_lines(&path, &[record.clone(), record.clone()]);
        let db = Database::memory().unwrap();
        let first = sync_rollout(&db, dir.path()).unwrap();
        assert_eq!((first.imported, first.skipped), (1, 1));

        // The file shrinks below the cursor (rewrite): read restarts from 0,
        // the receipt blocks the already-imported record.
        fs::write(&path, format!("{record}\n")).unwrap();
        let file = fs::OpenOptions::new().append(true).open(&path).unwrap();
        file.set_modified(std::time::SystemTime::now() + std::time::Duration::from_secs(5))
            .unwrap();
        drop(file);
        let result = sync_rollout(&db, dir.path()).unwrap();
        assert_eq!((result.imported, result.skipped), (0, 1));
        assert_eq!(rows(&db).len(), 1);
    }

    #[test]
    fn zcode_usage_separates_retry_attempts() {
        let dir = tempfile::tempdir().unwrap();
        let mut retried = model_io("req-1", 11, usage(2000, 20, 200, 0));
        // Oversized duration clamps to one hour.
        retried["durationMs"] = serde_json::json!(9_000_000_000_i64);
        write_lines(
            &session_file(dir.path()),
            &[model_io("req-1", 1, usage(1000, 10, 100, 0)), retried],
        );
        let db = Database::memory().unwrap();
        assert_eq!(sync_rollout(&db, dir.path()).unwrap().imported, 2);
        let imported = rows(&db);
        assert_eq!(
            imported[0].0,
            "zcode:sess_335a06d7-c664-47b7-8d3d-073596fc7a9d:req-1:1"
        );
        assert_eq!((imported[0].3, imported[0].7), (1000, 76309));
        assert_eq!(
            imported[1].0,
            "zcode:sess_335a06d7-c664-47b7-8d3d-073596fc7a9d:req-1:11"
        );
        assert_eq!((imported[1].3, imported[1].7), (2000, 3_600_000));
    }

    #[test]
    fn zcode_usage_skips_records_without_usage() {
        let dir = tempfile::tempdir().unwrap();
        // sample-3 shape: response without a usable usage object.
        let empty_usage = model_io("req-1", 1, serde_json::json!({}));
        let mut no_usage = model_io("req-2", 1, serde_json::json!({}));
        no_usage["response"]
            .as_object_mut()
            .unwrap()
            .remove("usage");
        let all_zero = model_io("req-3", 1, usage(0, 0, 0, 0));
        write_lines(
            &session_file(dir.path()),
            &[empty_usage, no_usage, all_zero],
        );
        let db = Database::memory().unwrap();
        let result = sync_rollout(&db, dir.path()).unwrap();
        assert_eq!((result.imported, result.skipped), (0, 0));
        assert!(rows(&db).is_empty());
    }

    #[test]
    fn zcode_usage_parses_iso8601_timestamps_and_model_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let mut fallback_model = model_io("req-1", 1, usage(1000, 10, 100, 0));
        fallback_model["model"]
            .as_object_mut()
            .unwrap()
            .remove("modelId");
        fallback_model["request"]["body"]["model"] = serde_json::json!("MiniMax-M3.1-Flash");
        let mut no_started = model_io("req-2", 1, usage(2000, 20, 200, 0));
        no_started.as_object_mut().unwrap().remove("startedAt");
        no_started["completedAt"] = serde_json::json!("2026-11-19T10:00:00Z");
        write_lines(&session_file(dir.path()), &[fallback_model, no_started]);
        let db = Database::memory().unwrap();
        assert_eq!(sync_rollout(&db, dir.path()).unwrap().imported, 2);
        let imported = rows(&db);
        // Model falls back to /request/body/model.
        assert_eq!(imported[0].1, "MiniMax-M3.1-Flash");
        assert_eq!(imported[0].8, epoch_seconds("2026-11-20T14:59:15.079Z"));
        // startedAt missing: completedAt (second precision, no fraction) is used.
        assert_eq!(imported[1].8, epoch_seconds("2026-11-19T10:00:00Z"));
    }

    #[test]
    fn zcode_usage_skips_missing_rollout_dir() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::memory().unwrap();
        let result = sync_rollout(&db, &dir.path().join("missing")).unwrap();
        assert_eq!(result.imported, 0);
        assert!(result.errors.is_empty());
    }

    #[test]
    fn zcode_usage_imports_model_usage_rows_from_db() {
        let home = tempfile::tempdir().unwrap();
        let source = zcode_source_db(home.path());
        insert_usage(
            &source,
            "usage_model_main_turn_msg_abc_0",
            "completed",
            T0_MS,
            76_309,
            121_664,
            4_142,
            0,
            16_832,
        );
        // error/cancelled rows are all-zero in practice (audit: 181 + 377).
        insert_usage(
            &source,
            "usage_model_main_turn_msg_err_0",
            "error",
            T0_MS + 1,
            500,
            0,
            0,
            0,
            0,
        );
        insert_usage(
            &source,
            "usage_model_main_turn_msg_can_0",
            "cancelled",
            T0_MS + 2,
            500,
            0,
            0,
            0,
            0,
        );
        drop(source);
        let db = Database::memory().unwrap();
        let result = sync_from_home(&db, home.path()).unwrap();
        assert_eq!(
            (result.imported, result.skipped, result.files_scanned),
            (1, 0, 1)
        );
        assert_eq!(
            rows(&db),
            vec![(
                "zcode:v2:usage_model_main_turn_msg_abc_0".into(),
                "GLM-5.3".into(),
                SESSION_ID.into(),
                121_664,
                4_142,
                16_832,
                0,
                76_309,
                T0_MS / 1000,
                "_zcode_session".into(),
                "zcode_db".into(),
                INPUT_TOKEN_SEMANTICS_TOTAL,
            )]
        );
    }

    #[test]
    fn zcode_usage_db_watermark_and_lookback() {
        let home = tempfile::tempdir().unwrap();
        let source = zcode_source_db(home.path());
        insert_usage(&source, "m1", "completed", T0_MS, 1_000, 100, 10, 0, 10);
        insert_usage(
            &source,
            "m2",
            "completed",
            T0_MS + 2_000,
            1_000,
            200,
            20,
            0,
            20,
        );
        drop(source);
        let db = Database::memory().unwrap();
        assert_eq!(sync_from_home(&db, home.path()).unwrap().imported, 2);

        // Unchanged db: the lookback re-scan hits receipts only.
        let again = sync_from_home(&db, home.path()).unwrap();
        assert_eq!((again.imported, again.skipped), (0, 2));
        assert_eq!(rows(&db).len(), 2);

        // A row committing late with started_at inside the lookback window is
        // still caught (watermark is rewound, not just advanced past).
        let source =
            rusqlite::Connection::open(home.path().join("cli").join("db").join("db.sqlite"))
                .unwrap();
        insert_usage(
            &source,
            "late",
            "completed",
            T0_MS + 1_000,
            1_000,
            50,
            5,
            0,
            5,
        );
        drop(source);
        let late = sync_from_home(&db, home.path()).unwrap();
        assert_eq!(late.imported, 1);
        assert_eq!(rows(&db).len(), 3);
    }

    #[test]
    fn zcode_usage_migrates_rollout_import_once() {
        let home = tempfile::tempdir().unwrap();
        let source = zcode_source_db(home.path());
        insert_usage(&source, "m1", "completed", T0_MS, 1_000, 100, 10, 0, 10);
        drop(source);
        let db = Database::memory().unwrap();
        // Seed exactly what the rollout importer left behind (audit: 81 log
        // rows + 81 receipts + 4 cursors, 3 of them pointing at deleted files).
        {
            let conn = db.conn.lock().unwrap();
            conn.execute(
                "INSERT INTO proxy_request_logs (
                    request_id, provider_id, app_type, model, input_tokens,
                    total_cost_usd, latency_ms, status_code, created_at, data_source
                 ) VALUES ('zcode:sess_old:uuid-1:1', '_zcode_session', 'zcode', 'GLM-5.3',
                           999, '0', 0, 200, 1, 'zcode_session')",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO session_usage_dedup (data_source, request_id, semantic_id, has_entry_id)
                 VALUES ('zcode_session', 'zcode:sess_old:uuid-1:1', 'zcode:sess_old:uuid-1:1', 1)",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO session_log_sync (file_path, last_modified, last_line_offset, last_synced_at)
                 VALUES ('zcode:v1:C:\\dead\\model-io-sess_old.jsonl', 1, 4096, 1)",
                [],
            )
            .unwrap();
        }

        let result = sync_from_home(&db, home.path()).unwrap();
        assert_eq!(result.imported, 1);
        let imported = rows(&db);
        assert_eq!(imported.len(), 1);
        assert_eq!(imported[0].0, "zcode:v2:m1");
        assert_eq!(
            scalar(
                &db,
                "SELECT COUNT(*) FROM session_usage_dedup WHERE data_source = 'zcode_session'"
            ),
            0
        );
        assert_eq!(
            scalar(
                &db,
                "SELECT COUNT(*) FROM session_usage_dedup WHERE data_source = 'zcode_db'"
            ),
            1
        );
        assert_eq!(
            scalar(
                &db,
                "SELECT COUNT(*) FROM session_log_sync WHERE file_path LIKE 'zcode:v1:%'"
            ),
            0
        );

        // No legacy markers remain: the purge must not run again (it would
        // reset the watermark and re-import the row).
        let again = sync_from_home(&db, home.path()).unwrap();
        assert_eq!(again.imported, 0);
    }

    #[test]
    fn zcode_usage_purge_resets_watermark_for_full_reimport() {
        let home = tempfile::tempdir().unwrap();
        let source = zcode_source_db(home.path());
        insert_usage(&source, "m1", "completed", T0_MS, 1_000, 100, 10, 0, 10);
        drop(source);
        let db = Database::memory().unwrap();
        sync_from_home(&db, home.path()).unwrap();

        // The db source went unreadable, the rollout fallback ran (its receipt
        // is the purge marker), and by the time db.sqlite is readable again
        // the watermark sits far past the db row. Keeping that watermark would
        // drop history older than the lookback window forever.
        let db_path = home.path().join("cli").join("db").join("db.sqlite");
        let watermark_key = format!("{DB_CURSOR_PREFIX}{}", db_path.display());
        update_sync_state(&db, &watermark_key, T0_MS + 10 * 3_600_000, 0).unwrap();
        {
            let conn = db.conn.lock().unwrap();
            conn.execute(
                "INSERT INTO session_usage_dedup (data_source, request_id, semantic_id, has_entry_id)
                 VALUES ('zcode_session', 'zcode:sess_fallback:uuid-9:1', 'zcode:sess_fallback:uuid-9:1', 1)",
                [],
            )
            .unwrap();
        }

        let result = sync_from_home(&db, home.path()).unwrap();
        assert_eq!(result.imported, 1);
        let imported = rows(&db);
        assert_eq!(imported.len(), 1);
        assert_eq!(imported[0].0, "zcode:v2:m1");
    }

    #[test]
    fn zcode_usage_falls_back_to_rollout_without_db() {
        let home = tempfile::tempdir().unwrap();
        let rollout = home.path().join("cli").join("rollout");
        fs::create_dir_all(&rollout).unwrap();
        write_lines(
            &session_file(&rollout),
            &[model_io("req-1", 1, usage(1000, 10, 100, 0))],
        );
        let db = Database::memory().unwrap();
        let result = sync_from_home(&db, home.path()).unwrap();
        assert_eq!(result.imported, 1);
        let imported = rows(&db);
        assert_eq!(
            imported[0].0,
            "zcode:sess_335a06d7-c664-47b7-8d3d-073596fc7a9d:req-1:1"
        );
        assert_eq!(imported[0].10, "zcode_session");

        // A db.sqlite without model_usage (not a ZCode database) also falls
        // back; the rollout cursor keeps the second scan a no-op.
        let source = zcode_source_db(home.path());
        source.execute_batch("DROP TABLE model_usage;").unwrap();
        drop(source);
        let result = sync_from_home(&db, home.path()).unwrap();
        assert_eq!(result.imported, 0);
        assert_eq!(rows(&db).len(), 1);
    }

    #[test]
    fn zcode_usage_reads_live_wal_database_readonly() {
        let home = tempfile::tempdir().unwrap();
        // The writer stays open across the sync: -wal/-shm stay live exactly
        // as while ZCode itself runs (the audited state of db.sqlite).
        let source = zcode_source_db(home.path());
        source
            .execute_batch("PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0;")
            .unwrap();
        insert_usage(
            &source,
            "wal_row",
            "completed",
            T0_MS,
            1_000,
            100,
            10,
            0,
            10,
        );
        let db = Database::memory().unwrap();
        let result = sync_from_home(&db, home.path()).unwrap();
        assert_eq!(result.imported, 1);
        assert_eq!(rows(&db)[0].0, "zcode:v2:wal_row");

        // The source connection is read-only: writes must fail.
        let reader = open_zcode_db(&home.path().join("cli").join("db").join("db.sqlite")).unwrap();
        assert!(reader.execute("DELETE FROM model_usage", []).is_err());
        drop(reader);
        drop(source);
    }

    /// One-off live check against the real `~/.zcode/cli/db/db.sqlite`
    /// (read-only, never writes cc-switch data). Run explicitly with
    /// `cargo test -- --ignored zcode_usage_live_db_smoke`.
    #[ignore = "touches the developer's real ZCode database"]
    #[test]
    fn zcode_usage_live_db_smoke() {
        let db_path = get_home_dir()
            .join(".zcode")
            .join("cli")
            .join("db")
            .join("db.sqlite");
        let source = open_zcode_db(&db_path)
            .unwrap_or_else(|| panic!("cannot open {} read-only", db_path.display()));
        let (count, total): (i64, i64) = source
            .query_row(
                "SELECT COUNT(*), COALESCE(SUM(input_tokens + output_tokens
                     + cache_creation_input_tokens + cache_read_input_tokens), 0)
                 FROM model_usage WHERE input_tokens + output_tokens
                     + cache_creation_input_tokens + cache_read_input_tokens > 0",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        println!("live model_usage: {count} nonzero rows, {total} four-way tokens");
        assert!(count > 100_000, "expected the full ~144k-row history");
        assert!(
            total > 35_000_000_000,
            "expected tens of billions of tokens"
        );
        assert!(source.execute("DELETE FROM model_usage", []).is_err());
    }
}
