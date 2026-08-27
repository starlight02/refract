//! 渠道仓储。
//!
//! 渠道与其协议端点是一个聚合根：读写总是整体进行，且必须在同一事务里，
//! 否则会出现「渠道已存在但端点还没写完」的中间态被路由层读到。

use refract_core::{
    Channel, ChannelEndpoint, ChannelId, ChannelKind, Credential, KeyStrategy, ModelEntry,
    Protocol, TranscodePolicy, UpstreamAddress,
};
use sqlx::Row;

use crate::db::{Database, StoreError};

/// 渠道仓储。
///
/// 可选持有静态加密主密钥:配置后凭据在落库前 AES-256-GCM 加密、读出后
/// 解密,对上层完全透明 —— 领域对象里永远是明文,库里永远是密文。
#[derive(Debug, Clone)]
pub struct ChannelRepo {
    db: Database,
    master_key: Option<[u8; 32]>,
}

/// 数据库里的渠道行。
struct ChannelRow {
    id: i64,
    owner_id: i64,
    name: String,
    kind: String,
    enabled: bool,
    priority: i64,
    weight: i64,
    credential: String,
    credentials: Option<String>,
    key_strategy: String,
    address: String,
    tags: String,
    timeout_secs: i64,
    proxy: Option<String>,
    param_override: Option<String>,
    note: Option<String>,
    auto_disabled: bool,
    balance: Option<f64>,
    balance_updated_at: Option<String>,
    extra_headers: Option<String>,
    test_model: Option<String>,
    empty_response_retry: Option<String>,
}

macro_rules! channel_cols {
    () => {
        "id, owner_id, name, kind, enabled, priority, weight, credential, credentials, \
         key_strategy, address, tags, timeout_secs, proxy, param_override, note, \
         auto_disabled, balance, balance_updated_at, extra_headers, test_model, \
         empty_response_retry"
    };
}

const SELECT_CHANNELS_LIST: &str = concat!(
    "SELECT ",
    channel_cols!(),
    " FROM channels WHERE owner_id = ? ORDER BY priority DESC, id ASC"
);

const SELECT_CHANNEL_BY_ID: &str = concat!(
    "SELECT ",
    channel_cols!(),
    " FROM channels WHERE owner_id = ? AND id = ?"
);
impl ChannelRepo {
    /// 绑定到一个数据库。
    pub fn new(db: Database) -> Self {
        Self {
            db,
            master_key: None,
        }
    }

    /// 注入静态加密主密钥。配置后凭据落库加密、读出解密,对调用方透明。
    pub fn with_master_key(mut self, key: Option<[u8; 32]>) -> Self {
        self.master_key = key;
        self
    }

    /// 写入前加密:无主密钥或已是密文时原样返回(防二次加密)。
    ///
    /// 加密失败是硬错误 —— 绝不允许凭据以明文落库:一旦明文写进库,
    /// 会随备份、`VACUUM`、导出等路径扩散,事后再想收回来代价极高。
    /// 配置了主密钥却加密失败说明运行环境有问题,应当立刻暴露给调用方。
    fn seal(&self, value: &str) -> Result<String, StoreError> {
        match self.master_key {
            Some(key) if !crate::crypto::is_encrypted(value) => {
                crate::crypto::encrypt_credential(value, &key).map_err(|error| {
                    tracing::error!(%error, "credential encryption failed, refusing to store plaintext");
                    StoreError::Encryption(error.to_string())
                })
            }
            _ => Ok(value.to_owned()),
        }
    }

    /// 读出后解密:无主密钥、无前缀或解密失败时按明文透传(向后兼容)。
    fn open(&self, stored: String) -> String {
        match self.master_key {
            Some(key) if crate::crypto::is_encrypted(&stored) => {
                match crate::crypto::decrypt_credential(&stored, &key) {
                    Ok(plain) => plain,
                    Err(error) => {
                        tracing::error!(%error, "credential decryption failed, passing through stored value");
                        stored
                    }
                }
            }
            _ => stored,
        }
    }

    /// 池写入:逐条加密后序列化;空池仍为 NULL。加密失败时整体中止。
    fn seal_pool(&self, credentials: &[Credential]) -> Result<Option<String>, StoreError> {
        if credentials.is_empty() {
            return Ok(None);
        }
        let mut sealed = Vec::with_capacity(credentials.len());
        for c in credentials {
            sealed.push(Credential::new(self.seal(c.expose())?));
        }
        Ok(Some(
            serde_json::to_string(&sealed).expect("credentials serialize"),
        ))
    }

    /// 池读出:反序列化后逐条解密。
    fn open_pool(&self, json: Option<String>) -> Result<Vec<Credential>, StoreError> {
        let pool = json
            .map(|s| serde_json::from_str::<Vec<Credential>>(&s))
            .transpose()
            .map_err(StoreError::json("channels.credentials"))?
            .unwrap_or_default();
        Ok(pool
            .into_iter()
            .map(|c| Credential::new(self.open(c.expose().to_owned())))
            .filter(|c| !c.is_empty())
            .collect())
    }

    /// 列出某个所有者的全部渠道（含端点）。
    pub async fn list(&self, owner_id: i64) -> Result<Vec<Channel>, StoreError> {
        let rows = sqlx::query(SELECT_CHANNELS_LIST)
            .bind(owner_id)
            .fetch_all(self.db.pool())
            .await?;

        let mut channels = Vec::with_capacity(rows.len());
        for row in rows {
            let parsed = Self::row_to_parts(&row)?;
            let endpoints = self.load_endpoints(parsed.id).await?;
            channels.push(self.assemble(parsed, endpoints)?);
        }
        Ok(channels)
    }

    /// 按 ID 取单个渠道。
    pub async fn get(&self, owner_id: i64, id: ChannelId) -> Result<Channel, StoreError> {
        let row = sqlx::query(SELECT_CHANNEL_BY_ID)
            .bind(owner_id)
            .bind(id)
            .fetch_optional(self.db.pool())
            .await?
            .ok_or_else(|| StoreError::not_found("channel", id))?;

        let parsed = Self::row_to_parts(&row)?;
        let endpoints = self.load_endpoints(id).await?;
        self.assemble(parsed, endpoints)
    }

    /// 新建渠道，返回带 ID 的完整对象。
    pub async fn create(&self, channel: &Channel) -> Result<Channel, StoreError> {
        channel
            .validate()
            .map_err(|e| StoreError::Invalid(e.to_string()))?;

        let mut tx = self.db.pool().begin().await?;
        let id = self.insert_channel(&mut tx, channel).await?;
        tx.commit().await?;

        self.get(channel.owner_id, id).await
    }

    /// 在给定事务里插入一个渠道及其全部端点,返回新 ID。
    async fn insert_channel(
        &self,
        tx: &mut sqlx::SqliteConnection,
        channel: &Channel,
    ) -> Result<i64, StoreError> {
        // 加密可能失败并中止写入，必须在构建 bind 链之前完成。
        let sealed_credential = self.seal(channel.credential.expose())?;
        let sealed_pool = self.seal_pool(&channel.credentials)?;
        let id: i64 = sqlx::query(
            "INSERT INTO channels \
             (owner_id, name, kind, enabled, priority, weight, credential, credentials, \
              key_strategy, address, tags, timeout_secs, proxy, param_override, note, \
              extra_headers, test_model, empty_response_retry) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) RETURNING id",
        )
        .bind(channel.owner_id)
        .bind(&channel.name)
        .bind(channel.kind.as_str())
        .bind(channel.enabled)
        .bind(i64::from(channel.priority))
        .bind(i64::from(channel.weight))
        .bind(sealed_credential)
        .bind(sealed_pool)
        .bind(channel.key_strategy.as_str())
        .bind(serde_json::to_string(&channel.address).expect("address serializes"))
        .bind(serde_json::to_string(&channel.tags).expect("tags serialize"))
        .bind(i64::from(channel.timeout_secs))
        .bind(channel.proxy.as_deref())
        .bind(
            channel
                .param_override
                .as_ref()
                .map(|v| serde_json::to_string(v).expect("param_override serializes")),
        )
        .bind(channel.note.as_deref())
        .bind(
            (!channel.extra_headers.is_empty())
                .then(|| serde_json::to_string(&channel.extra_headers).expect("headers serialize")),
        )
        .bind(channel.test_model.as_deref())
        .bind((!channel.empty_response_retry.is_inherited()).then(|| {
            serde_json::to_string(&channel.empty_response_retry)
                .expect("empty response retry override serializes")
        }))
        .fetch_one(&mut *tx)
        .await?
        .get(0);

        for ep in &channel.endpoints {
            self.insert_endpoint(&mut *tx, id, ep).await?;
        }
        Ok(id)
    }

    /// 原子替换该所有者的全部渠道（导入的 replace 模式用），返回导入数。
    ///
    /// 删旧与插新在同一个事务里：分成两步独立提交的话，中途失败会留下一个
    /// 「渠道被清空但只导入了一半」的实例 —— 那比导入失败更糟，用户说不清
    /// 自己现在处于什么状态。任何一个渠道无效则整体回滚。
    pub async fn replace_all(
        &self,
        owner_id: i64,
        channels: &[Channel],
    ) -> Result<u32, StoreError> {
        for channel in channels {
            channel
                .validate()
                .map_err(|e| StoreError::Invalid(e.to_string()))?;
        }

        let mut tx = self.db.pool().begin().await?;
        sqlx::query("DELETE FROM channels WHERE owner_id = ?")
            .bind(owner_id)
            .execute(&mut *tx)
            .await?;
        let mut imported = 0_u32;
        for channel in channels {
            self.insert_channel(&mut tx, channel).await?;
            imported += 1;
        }
        tx.commit().await?;
        Ok(imported)
    }

    /// 批量改启用状态，返回实际命中的行数。
    ///
    /// 单条 SQL 完成：天然原子，且对「列表里有已被删除的 ID」宽容 ——
    /// 另一个标签页刚删掉的渠道不该让整批操作失败。
    pub async fn set_enabled_many(
        &self,
        owner_id: i64,
        ids: &[ChannelId],
        enabled: bool,
    ) -> Result<u64, StoreError> {
        if ids.is_empty() {
            return Ok(0);
        }
        // 动态部分只有 `?` 占位符序列，值全部走绑定，无注入面。
        let placeholders = vec!["?"; ids.len()].join(", ");
        let sql = format!(
            // 与单条 set_enabled 一致：手动操作（无论启停）都清自动禁用标记。
            "UPDATE channels SET enabled = ?, auto_disabled = 0, updated_at = datetime('now') \
             WHERE owner_id = ? AND id IN ({placeholders})"
        );
        let mut query = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(enabled)
            .bind(owner_id);
        for id in ids {
            query = query.bind(id);
        }
        Ok(query.execute(self.db.pool()).await?.rows_affected())
    }

    /// 批量删除，返回实际删除的行数。端点靠外键级联删除。
    pub async fn delete_many(&self, owner_id: i64, ids: &[ChannelId]) -> Result<u64, StoreError> {
        if ids.is_empty() {
            return Ok(0);
        }
        // 同上：动态部分只有占位符序列。
        let placeholders = vec!["?"; ids.len()].join(", ");
        let sql = format!("DELETE FROM channels WHERE owner_id = ? AND id IN ({placeholders})");
        let mut query = sqlx::query(sqlx::AssertSqlSafe(sql)).bind(owner_id);
        for id in ids {
            query = query.bind(id);
        }
        Ok(query.execute(self.db.pool()).await?.rows_affected())
    }

    /// 全量更新一个渠道及其端点。
    ///
    /// 端点采用「删旧插新」而非增量 diff：端点数量最多 4 个，diff 的复杂度
    /// 远超其收益，且删插能天然处理协议变更这种会撞唯一约束的情况。
    pub async fn update(&self, channel: &Channel) -> Result<Channel, StoreError> {
        channel
            .validate()
            .map_err(|e| StoreError::Invalid(e.to_string()))?;

        let sealed_credential = self.seal(channel.credential.expose())?;
        let sealed_pool = self.seal_pool(&channel.credentials)?;
        let mut tx = self.db.pool().begin().await?;
        let affected = sqlx::query(
            "UPDATE channels SET name = ?, kind = ?, enabled = ?, priority = ?, weight = ?, \
             credential = ?, credentials = ?, key_strategy = ?, address = ?, tags = ?, \
             timeout_secs = ?, proxy = ?, param_override = ?, note = ?, extra_headers = ?, \
             test_model = ?, empty_response_retry = ?, \
             updated_at = datetime('now') \
             WHERE id = ? AND owner_id = ?",
        )
        .bind(&channel.name)
        .bind(channel.kind.as_str())
        .bind(channel.enabled)
        .bind(i64::from(channel.priority))
        .bind(i64::from(channel.weight))
        .bind(sealed_credential)
        .bind(sealed_pool)
        .bind(channel.key_strategy.as_str())
        .bind(serde_json::to_string(&channel.address).expect("address serializes"))
        .bind(serde_json::to_string(&channel.tags).expect("tags serialize"))
        .bind(i64::from(channel.timeout_secs))
        .bind(channel.proxy.as_deref())
        .bind(
            channel
                .param_override
                .as_ref()
                .map(|v| serde_json::to_string(v).expect("param_override serializes")),
        )
        .bind(channel.note.as_deref())
        .bind(
            (!channel.extra_headers.is_empty())
                .then(|| serde_json::to_string(&channel.extra_headers).expect("headers serialize")),
        )
        .bind(channel.test_model.as_deref())
        .bind((!channel.empty_response_retry.is_inherited()).then(|| {
            serde_json::to_string(&channel.empty_response_retry)
                .expect("empty response retry override serializes")
        }))
        .bind(channel.id)
        .bind(channel.owner_id)
        .execute(&mut *tx)
        .await?
        .rows_affected();

        crate::ensure_affected(affected, "channel", channel.id)?;

        sqlx::query("DELETE FROM channel_endpoints WHERE channel_id = ?")
            .bind(channel.id)
            .execute(&mut *tx)
            .await?;
        for ep in &channel.endpoints {
            self.insert_endpoint(&mut tx, channel.id, ep).await?;
        }
        tx.commit().await?;

        self.get(channel.owner_id, channel.id).await
    }

    /// 删除渠道。端点靠外键级联删除。
    pub async fn delete(&self, owner_id: i64, id: ChannelId) -> Result<(), StoreError> {
        let affected = sqlx::query("DELETE FROM channels WHERE id = ? AND owner_id = ?")
            .bind(id)
            .bind(owner_id)
            .execute(self.db.pool())
            .await?
            .rows_affected();
        crate::ensure_affected(affected, "channel", id)?;
        Ok(())
    }

    /// 删除该所有者的全部渠道（导入的 replace 模式用）。端点随外键级联删除。
    pub async fn delete_all(&self, owner_id: i64) -> Result<u64, StoreError> {
        let affected = sqlx::query("DELETE FROM channels WHERE owner_id = ?")
            .bind(owner_id)
            .execute(self.db.pool())
            .await?
            .rows_affected();
        Ok(affected)
    }

    /// 只改启用状态。这是列表页最高频的操作，值得一条独立的窄更新。
    pub async fn set_enabled(
        &self,
        owner_id: i64,
        id: ChannelId,
        enabled: bool,
    ) -> Result<(), StoreError> {
        // 手动改启用状态时清掉自动禁用标记：用户显式启用等于「我知道并
        // 认为它好了」，显式禁用则转为手动禁用（不再参与重测自愈）。
        let affected = sqlx::query(
            "UPDATE channels SET enabled = ?, auto_disabled = 0, updated_at = datetime('now') \
             WHERE id = ? AND owner_id = ?",
        )
        .bind(enabled)
        .bind(id)
        .bind(owner_id)
        .execute(self.db.pool())
        .await?
        .rows_affected();
        crate::ensure_affected(affected, "channel", id)?;
        Ok(())
    }

    /// 记录一次余额探测结果。观测数据的窄更新，不触碰配置。
    pub async fn set_balance(
        &self,
        owner_id: i64,
        id: ChannelId,
        balance: f64,
    ) -> Result<(), StoreError> {
        let affected = sqlx::query(
            "UPDATE channels SET balance = ?, balance_updated_at = ? \
             WHERE id = ? AND owner_id = ?",
        )
        .bind(balance)
        .bind(chrono::Utc::now().to_rfc3339())
        .bind(id)
        .bind(owner_id)
        .execute(self.db.pool())
        .await?
        .rows_affected();
        crate::ensure_affected(affected, "channel", id)?;
        Ok(())
    }

    /// 因终态错误（凭据失效等）自动禁用渠道。
    ///
    /// 与手动禁用的区别：保留自愈资格 —— 定时重测成功后会自动恢复。
    pub async fn set_auto_disabled(&self, owner_id: i64, id: ChannelId) -> Result<(), StoreError> {
        let affected = sqlx::query(
            "UPDATE channels SET enabled = 0, auto_disabled = 1, updated_at = datetime('now') \
             WHERE id = ? AND owner_id = ?",
        )
        .bind(id)
        .bind(owner_id)
        .execute(self.db.pool())
        .await?
        .rows_affected();
        crate::ensure_affected(affected, "channel", id)?;
        Ok(())
    }
    /// 自动禁用的渠道经重测成功后恢复。只作用于 `auto_disabled` 的行 ——
    /// 手动禁用的渠道即使重测通过也不动，那是用户的显式决定。
    pub async fn restore_auto_disabled(
        &self,
        owner_id: i64,
        id: ChannelId,
    ) -> Result<bool, StoreError> {
        let affected = sqlx::query(
            "UPDATE channels SET enabled = 1, auto_disabled = 0, updated_at = datetime('now') \
             WHERE id = ? AND owner_id = ? AND auto_disabled = 1",
        )
        .bind(id)
        .bind(owner_id)
        .execute(self.db.pool())
        .await?
        .rows_affected();
        Ok(affected > 0)
    }

    async fn load_endpoints(&self, channel_id: i64) -> Result<Vec<ChannelEndpoint>, StoreError> {
        let rows = sqlx::query(
            "SELECT protocol, sort_order, enabled, address, credential, models, transcode \
             FROM channel_endpoints WHERE channel_id = ? ORDER BY sort_order ASC, id ASC",
        )
        .bind(channel_id)
        .fetch_all(self.db.pool())
        .await?;

        let mut out = Vec::with_capacity(rows.len());
        for row in rows {
            let protocol: String = row.get("protocol");
            let protocol: Protocol = protocol
                .parse()
                .map_err(|_| StoreError::Invalid(format!("unknown protocol `{protocol}`")))?;
            let address: String = row.get("address");
            let models: String = row.get("models");
            let transcode: String = row.get("transcode");
            let credential: Option<String> = row.get("credential");
            let sort_order: i64 = row.get("sort_order");

            out.push(ChannelEndpoint {
                protocol,
                order: sort_order.clamp(0, i64::from(u16::MAX)) as u16,
                enabled: row.get("enabled"),
                address: serde_json::from_str::<UpstreamAddress>(&address)
                    .map_err(StoreError::json("channel_endpoints.address"))?,
                credential: credential.map(|c| Credential::new(self.open(c))),
                models: serde_json::from_str::<Vec<ModelEntry>>(&models)
                    .map_err(StoreError::json("channel_endpoints.models"))?,
                transcode: serde_json::from_str::<TranscodePolicy>(&transcode)
                    .map_err(StoreError::json("channel_endpoints.transcode"))?,
            });
        }
        Ok(out)
    }

    async fn insert_endpoint(
        &self,
        tx: &mut sqlx::SqliteConnection,
        channel_id: i64,
        ep: &ChannelEndpoint,
    ) -> Result<(), StoreError> {
        let sealed_credential = ep
            .credential
            .as_ref()
            .map(|c| self.seal(c.expose()))
            .transpose()?;
        sqlx::query(
            "INSERT INTO channel_endpoints \
             (channel_id, protocol, sort_order, enabled, address, credential, models, transcode) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(channel_id)
        .bind(ep.protocol.as_str())
        .bind(i64::from(ep.order))
        .bind(ep.enabled)
        .bind(serde_json::to_string(&ep.address).expect("address serializes"))
        .bind(sealed_credential)
        .bind(serde_json::to_string(&ep.models).expect("models serialize"))
        .bind(serde_json::to_string(&ep.transcode).expect("transcode serializes"))
        .execute(&mut *tx)
        .await?;
        Ok(())
    }

    fn row_to_parts(row: &sqlx::sqlite::SqliteRow) -> Result<ChannelRow, StoreError> {
        Ok(ChannelRow {
            id: row.get("id"),
            owner_id: row.get("owner_id"),
            name: row.get("name"),
            kind: row.get("kind"),
            enabled: row.get("enabled"),
            priority: row.get("priority"),
            weight: row.get("weight"),
            credential: row.get("credential"),
            credentials: row.get("credentials"),
            key_strategy: row.get("key_strategy"),
            address: row.get("address"),
            tags: row.get("tags"),
            timeout_secs: row.get("timeout_secs"),
            proxy: row.get("proxy"),
            param_override: row.get("param_override"),
            note: row.get("note"),
            auto_disabled: row.get("auto_disabled"),
            balance: row.get("balance"),
            balance_updated_at: row.get("balance_updated_at"),
            extra_headers: row.get("extra_headers"),
            test_model: row.get("test_model"),
            empty_response_retry: row.get("empty_response_retry"),
        })
    }

    fn assemble(
        &self,
        row: ChannelRow,
        endpoints: Vec<ChannelEndpoint>,
    ) -> Result<Channel, StoreError> {
        let kind: ChannelKind = row
            .kind
            .parse()
            .map_err(|_| StoreError::Invalid(format!("unknown channel kind `{}`", row.kind)))?;

        Ok(Channel {
            id: row.id,
            owner_id: row.owner_id,
            name: row.name,
            kind,
            enabled: row.enabled,
            priority: row.priority.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32,
            weight: row.weight.clamp(0, i64::from(u32::MAX)) as u32,
            credential: Credential::new(self.open(row.credential)),
            credentials: self.open_pool(row.credentials)?,
            key_strategy: KeyStrategy::parse(&row.key_strategy),
            address: serde_json::from_str(&row.address)
                .map_err(StoreError::json("channels.address"))?,
            endpoints,
            tags: serde_json::from_str(&row.tags).map_err(StoreError::json("channels.tags"))?,
            timeout_secs: row.timeout_secs.clamp(0, i64::from(u32::MAX)) as u32,
            proxy: row.proxy,
            param_override: row
                .param_override
                .map(|s| serde_json::from_str(&s))
                .transpose()
                .map_err(StoreError::json("channels.param_override"))?,
            note: row.note,
            auto_disabled: row.auto_disabled,
            balance: row.balance,
            balance_updated_at: row
                .balance_updated_at
                .and_then(|s| chrono::DateTime::parse_from_rfc3339(&s).ok())
                .map(|d| d.with_timezone(&chrono::Utc)),
            extra_headers: row
                .extra_headers
                .map(|s| serde_json::from_str(&s))
                .transpose()
                .map_err(StoreError::json("channels.extra_headers"))?
                .unwrap_or_default(),
            test_model: row.test_model,
            empty_response_retry: row
                .empty_response_retry
                .map(|value| serde_json::from_str(&value))
                .transpose()
                .map_err(StoreError::json("channels.empty_response_retry"))?
                .unwrap_or_default(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use refract_core::{DEFAULT_OWNER_ID, EmptyResponseRetryOverride, ParamOverride, ProtocolSet};

    async fn repo() -> ChannelRepo {
        ChannelRepo::new(Database::open_in_memory().await.unwrap())
    }

    fn sample_single() -> Channel {
        Channel {
            id: 0,
            owner_id: DEFAULT_OWNER_ID,
            name: "openai-official".into(),
            kind: ChannelKind::Single(Protocol::Chat),
            enabled: true,
            priority: 10,
            weight: 3,
            credential: Credential::new("sk-channel-default"),
            credentials: Vec::new(),
            key_strategy: KeyStrategy::default(),
            address: UpstreamAddress::default(),
            endpoints: vec![ChannelEndpoint {
                models: vec![
                    ModelEntry::plain("gpt-4o"),
                    ModelEntry::mapped("fast", "gpt-4o-mini"),
                ],
                ..ChannelEndpoint::new(Protocol::Chat)
            }],
            tags: vec!["prod".into()],
            timeout_secs: 60,
            proxy: None,
            param_override: None,
            note: Some("main".into()),
            auto_disabled: false,
            balance: None,
            balance_updated_at: None,
            extra_headers: Vec::new(),
            test_model: None,
            empty_response_retry: EmptyResponseRetryOverride {
                window_secs: Some(6),
                max_retries: Some(2),
            },
        }
    }

    fn sample_aggregate() -> Channel {
        Channel {
            kind: ChannelKind::Aggregate,
            name: "relay-multi".into(),
            endpoints: vec![
                ChannelEndpoint {
                    order: 0,
                    models: vec![ModelEntry::plain("claude-sonnet-4-6")],
                    credential: Some(Credential::new("sk-ant-endpoint")),
                    address: UpstreamAddress {
                        unofficial: true,
                        base_url: Some("https://relay.example.com/anthropic".into()),
                        ..Default::default()
                    },
                    transcode: TranscodePolicy {
                        enabled: true,
                        accepted: ProtocolSet::from_iter_protocols([Protocol::Chat]),
                    },
                    ..ChannelEndpoint::new(Protocol::Messages)
                },
                ChannelEndpoint {
                    order: 1,
                    models: vec![ModelEntry::plain("gpt-4o")],
                    ..ChannelEndpoint::new(Protocol::Chat)
                },
            ],
            ..sample_single()
        }
    }

    #[tokio::test]
    async fn create_and_get_roundtrip_single_channel() {
        let repo = repo().await;
        let created = repo.create(&sample_single()).await.unwrap();
        assert!(created.id > 0);

        let fetched = repo.get(DEFAULT_OWNER_ID, created.id).await.unwrap();
        assert_eq!(fetched, created);
        assert_eq!(fetched.name, "openai-official");
        assert_eq!(fetched.priority, 10);
        assert_eq!(fetched.weight, 3);
        assert_eq!(fetched.endpoints.len(), 1);
        assert_eq!(fetched.endpoints[0].models.len(), 2);
        assert_eq!(
            fetched.endpoints[0].models[1].upstream_name(),
            "gpt-4o-mini"
        );
    }

    #[tokio::test]
    async fn aggregate_channel_preserves_per_endpoint_config() {
        let repo = repo().await;
        let created = repo.create(&sample_aggregate()).await.unwrap();
        let fetched = repo.get(DEFAULT_OWNER_ID, created.id).await.unwrap();

        assert_eq!(fetched.endpoints.len(), 2);
        let msg = &fetched.endpoints[0];
        assert_eq!(msg.protocol, Protocol::Messages);
        assert_eq!(msg.order, 0);
        // 端点独立的 key 与地址必须原样存回。
        assert_eq!(msg.credential.as_ref().unwrap().expose(), "sk-ant-endpoint");
        assert_eq!(
            msg.address.base_url.as_deref(),
            Some("https://relay.example.com/anthropic")
        );
        // 端点独立的转换策略也要。
        assert!(msg.transcode.enabled);
        assert!(msg.transcode.accepted.contains(Protocol::Chat));

        let chat = &fetched.endpoints[1];
        assert_eq!(chat.protocol, Protocol::Chat);
        assert!(chat.credential.is_none(), "unset endpoint key stays unset");
    }

    #[tokio::test]
    async fn endpoints_come_back_in_order() {
        let repo = repo().await;
        let mut ch = sample_aggregate();
        // 故意把 order 大的放前面，读回来应当按 order 排序。
        ch.endpoints[0].order = 9;
        ch.endpoints[1].order = 1;
        let created = repo.create(&ch).await.unwrap();
        let fetched = repo.get(DEFAULT_OWNER_ID, created.id).await.unwrap();
        assert_eq!(
            fetched
                .endpoints
                .iter()
                .map(|e| e.protocol)
                .collect::<Vec<_>>(),
            vec![Protocol::Chat, Protocol::Messages]
        );
    }

    #[tokio::test]
    async fn update_replaces_endpoints_wholesale() {
        let repo = repo().await;
        let mut ch = repo.create(&sample_aggregate()).await.unwrap();

        // 换掉端点集合：去掉 chat，把 messages 换成 gemini。
        ch.endpoints = vec![ChannelEndpoint {
            models: vec![ModelEntry::plain("gemini-2.5-pro")],
            ..ChannelEndpoint::new(Protocol::Gemini)
        }];
        ch.name = "renamed".into();
        let updated = repo.update(&ch).await.unwrap();

        assert_eq!(updated.name, "renamed");
        assert_eq!(updated.endpoints.len(), 1);
        assert_eq!(updated.endpoints[0].protocol, Protocol::Gemini);
    }

    #[tokio::test]
    async fn update_can_change_endpoint_protocol_without_unique_conflict() {
        // 删旧插新的关键收益：协议变更不会撞 UNIQUE(channel_id, protocol)。
        let repo = repo().await;
        let mut ch = repo.create(&sample_single()).await.unwrap();
        ch.kind = ChannelKind::Single(Protocol::Messages);
        ch.endpoints[0].protocol = Protocol::Messages;
        let updated = repo.update(&ch).await.unwrap();
        assert_eq!(updated.endpoints[0].protocol, Protocol::Messages);
    }

    #[tokio::test]
    async fn invalid_channel_is_rejected_before_touching_db() {
        let repo = repo().await;
        let mut ch = sample_single();
        ch.endpoints.push(ChannelEndpoint::new(Protocol::Gemini));
        let err = repo.create(&ch).await.unwrap_err();
        assert!(matches!(err, StoreError::Invalid(_)), "{err:?}");

        let all = repo.list(DEFAULT_OWNER_ID).await.unwrap();
        assert!(all.is_empty(), "rejected channel must not be persisted");
    }

    #[tokio::test]
    async fn delete_removes_channel_and_endpoints() {
        let repo = repo().await;
        let created = repo.create(&sample_aggregate()).await.unwrap();
        repo.delete(DEFAULT_OWNER_ID, created.id).await.unwrap();

        assert!(repo.get(DEFAULT_OWNER_ID, created.id).await.is_err());
        let leftover: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM channel_endpoints")
            .fetch_one(repo.db.pool())
            .await
            .unwrap();
        assert_eq!(leftover.0, 0);
    }

    #[tokio::test]
    async fn delete_missing_channel_reports_not_found() {
        let repo = repo().await;
        let err = repo.delete(DEFAULT_OWNER_ID, 999).await.unwrap_err();
        assert!(matches!(err, StoreError::NotFound { .. }), "{err:?}");
    }

    #[tokio::test]
    async fn list_orders_by_priority_desc() {
        let repo = repo().await;
        let mut low = sample_single();
        low.name = "low".into();
        low.priority = 1;
        let mut high = sample_single();
        high.name = "high".into();
        high.priority = 100;
        repo.create(&low).await.unwrap();
        repo.create(&high).await.unwrap();

        let all = repo.list(DEFAULT_OWNER_ID).await.unwrap();
        assert_eq!(
            all.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(),
            vec!["high", "low"]
        );
    }

    #[tokio::test]
    async fn owner_scoping_hides_other_owners_channels() {
        // 单用户系统，但 owner 隔离现在就要生效，将来加多用户才不会漏。
        let repo = repo().await;
        let mut other = sample_single();
        other.owner_id = 2;
        repo.create(&other).await.unwrap();

        assert!(repo.list(DEFAULT_OWNER_ID).await.unwrap().is_empty());
        assert_eq!(repo.list(2).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn set_enabled_toggles_only_that_flag() {
        let repo = repo().await;
        let created = repo.create(&sample_single()).await.unwrap();
        repo.set_enabled(DEFAULT_OWNER_ID, created.id, false)
            .await
            .unwrap();

        let fetched = repo.get(DEFAULT_OWNER_ID, created.id).await.unwrap();
        assert!(!fetched.enabled);
        assert_eq!(fetched.name, created.name);
        assert_eq!(fetched.endpoints, created.endpoints);
    }

    #[tokio::test]
    async fn replace_all_swaps_the_whole_set_atomically() {
        let repo = repo().await;
        repo.create(&sample_single()).await.unwrap();

        let mut incoming = sample_aggregate();
        incoming.name = "restored".into();
        let imported = repo
            .replace_all(DEFAULT_OWNER_ID, &[incoming])
            .await
            .unwrap();
        assert_eq!(imported, 1);

        let all = repo.list(DEFAULT_OWNER_ID).await.unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].name, "restored");
    }

    #[tokio::test]
    async fn replace_all_rolls_back_when_any_channel_is_invalid() {
        let repo = repo().await;
        repo.create(&sample_single()).await.unwrap();

        let mut bad = sample_single();
        bad.name = "".into(); // 无效：名字为空。
        let err = repo
            .replace_all(DEFAULT_OWNER_ID, &[sample_aggregate(), bad])
            .await
            .unwrap_err();
        assert!(matches!(err, StoreError::Invalid(_)), "{err:?}");

        // 整体回滚：既没有清空旧渠道，也没有导入前半批。
        let all = repo.list(DEFAULT_OWNER_ID).await.unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].name, "openai-official");
    }

    #[tokio::test]
    async fn set_enabled_many_tolerates_missing_ids() {
        let repo = repo().await;
        let a = repo.create(&sample_single()).await.unwrap();
        let mut second = sample_single();
        second.name = "second".into();
        let b = repo.create(&second).await.unwrap();

        // 与单条 set_enabled 语义对齐：批量手动启停也要清自动禁用标记。
        repo.set_auto_disabled(DEFAULT_OWNER_ID, a.id)
            .await
            .unwrap();

        let affected = repo
            .set_enabled_many(DEFAULT_OWNER_ID, &[a.id, b.id, 9_999], false)
            .await
            .unwrap();
        assert_eq!(affected, 2, "不存在的 ID 不算命中也不报错");

        let all = repo.list(DEFAULT_OWNER_ID).await.unwrap();
        assert!(all.iter().all(|c| !c.enabled));
        assert!(
            all.iter().all(|c| !c.auto_disabled),
            "批量手动禁用必须清掉 auto_disabled"
        );
    }

    #[tokio::test]
    async fn delete_many_removes_only_listed_channels() {
        let repo = repo().await;
        let a = repo.create(&sample_single()).await.unwrap();
        let mut second = sample_single();
        second.name = "keep".into();
        let b = repo.create(&second).await.unwrap();

        let affected = repo
            .delete_many(DEFAULT_OWNER_ID, &[a.id, 12_345])
            .await
            .unwrap();
        assert_eq!(affected, 1);

        let all = repo.list(DEFAULT_OWNER_ID).await.unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].id, b.id);
    }

    #[tokio::test]
    async fn bulk_helpers_respect_owner_scoping() {
        let repo = repo().await;
        let mut other = sample_single();
        other.owner_id = 2;
        let created = repo.create(&other).await.unwrap();

        // 用错误的 owner 批量操作：一个都不该命中。
        assert_eq!(
            repo.set_enabled_many(DEFAULT_OWNER_ID, &[created.id], false)
                .await
                .unwrap(),
            0
        );
        assert_eq!(
            repo.delete_many(DEFAULT_OWNER_ID, &[created.id])
                .await
                .unwrap(),
            0
        );
        assert_eq!(repo.list(2).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn address_and_param_override_survive_roundtrip() {
        let repo = repo().await;
        let mut ch = sample_single();
        ch.address = UpstreamAddress {
            unofficial: true,
            full_address: true,
            base_url: Some("https://odd.example.com/inference".into()),
            version_prefix: None,
            path: None,
        };
        ch.param_override = Some(
            serde_json::from_value::<ParamOverride>(
                serde_json::json!({"common": {"temperature": 0.2}}),
            )
            .unwrap(),
        );
        let created = repo.create(&ch).await.unwrap();
        let fetched = repo.get(DEFAULT_OWNER_ID, created.id).await.unwrap();

        assert!(fetched.address.full_address);
        assert_eq!(
            fetched.address.base_url.as_deref(),
            Some("https://odd.example.com/inference")
        );
        assert_eq!(
            fetched.param_override.unwrap().common["temperature"],
            serde_json::json!(0.2)
        );
    }

    #[tokio::test]
    async fn legacy_flat_param_override_loads_as_common() {
        let repo = repo().await;
        let created = repo.create(&sample_single()).await.unwrap();
        sqlx::query("UPDATE channels SET param_override = ? WHERE id = ?")
            .bind(r#"{"temperature":0.2,"chat":{"top_p":0.9}}"#)
            .bind(created.id)
            .execute(repo.db.pool())
            .await
            .unwrap();

        let fetched = repo.get(DEFAULT_OWNER_ID, created.id).await.unwrap();
        let override_ = fetched.param_override.expect("legacy row must load");
        assert_eq!(override_.common["temperature"], serde_json::json!(0.2));
        assert_eq!(
            override_.protocols[&refract_core::Protocol::Chat]["top_p"],
            serde_json::json!(0.9)
        );
    }

    /// 直接查库里的原始凭据列,绕过仓储的解密层。
    async fn raw_channel_credentials(repo: &ChannelRepo, id: i64) -> (String, Option<String>) {
        let row = sqlx::query("SELECT credential, credentials FROM channels WHERE id = ?")
            .bind(id)
            .fetch_one(repo.db.pool())
            .await
            .unwrap();
        (row.get("credential"), row.get("credentials"))
    }

    #[tokio::test]
    async fn without_master_key_credentials_remain_plaintext() {
        let repo = repo().await;
        let mut sample = sample_single();
        sample.credential = Credential::new("sk-plain-secret");
        sample.credentials = vec![Credential::new("sk-pool-1"), Credential::new("sk-pool-2")];
        let created = repo.create(&sample).await.unwrap();

        let (raw_single, raw_pool) = raw_channel_credentials(&repo, created.id).await;
        assert_eq!(raw_single, "sk-plain-secret");
        assert!(!crate::crypto::is_encrypted(&raw_single));
        let raw_pool_str = raw_pool.unwrap();
        assert!(raw_pool_str.contains("sk-pool-1"));

        let fetched = repo.get(DEFAULT_OWNER_ID, created.id).await.unwrap();
        assert_eq!(fetched.credential.expose(), "sk-plain-secret");
        assert_eq!(fetched.credentials.len(), 2);
        assert_eq!(fetched.credentials[0].expose(), "sk-pool-1");
    }

    #[tokio::test]
    async fn with_master_key_credentials_are_encrypted_in_db_and_decrypted_on_read() {
        let key = [42_u8; 32];
        let db = Database::open_in_memory().await.unwrap();
        let repo = ChannelRepo::new(db.clone()).with_master_key(Some(key));

        let mut sample = sample_single();
        sample.credential = Credential::new("sk-super-secret");
        sample.credentials = vec![Credential::new("sk-pool-a"), Credential::new("sk-pool-b")];
        let created = repo.create(&sample).await.unwrap();

        // 数据库底层必须是 refract.v1. 密文,绝不能出现明文。
        let (raw_single, raw_pool) = raw_channel_credentials(&repo, created.id).await;
        assert!(crate::crypto::is_encrypted(&raw_single));
        assert!(!raw_single.contains("sk-super-secret"));
        let raw_pool_str = raw_pool.unwrap();
        assert!(crate::crypto::is_encrypted(&raw_pool_str) || raw_pool_str.contains("refract.v1."));
        assert!(!raw_pool_str.contains("sk-pool-a"));

        // 读回时对上层透明解密为明文。
        let fetched = repo.get(DEFAULT_OWNER_ID, created.id).await.unwrap();
        assert_eq!(fetched.credential.expose(), "sk-super-secret");
        assert_eq!(fetched.credentials.len(), 2);
        assert_eq!(fetched.credentials[0].expose(), "sk-pool-a");
        assert_eq!(fetched.credentials[1].expose(), "sk-pool-b");

        // 更新同一渠道,密文不被二次加密(防 double-seal)。
        let mut updated = fetched;
        updated.name = "renamed".into();
        let saved = repo.update(&updated).await.unwrap();
        assert_eq!(saved.credential.expose(), "sk-super-secret");
        let (raw_single_after, _) = raw_channel_credentials(&repo, saved.id).await;
        assert!(crate::crypto::is_encrypted(&raw_single_after));
        // 解密仍正常。
        let refetched = repo.get(DEFAULT_OWNER_ID, saved.id).await.unwrap();
        assert_eq!(refetched.credential.expose(), "sk-super-secret");
    }

    #[tokio::test]
    async fn legacy_plaintext_row_is_decrypted_transparently_when_key_added_later() {
        let db = Database::open_in_memory().await.unwrap();
        let plain_repo = ChannelRepo::new(db.clone());

        let mut sample = sample_single();
        sample.credential = Credential::new("sk-legacy-plain");
        let created = plain_repo.create(&sample).await.unwrap();

        // 之后部署配置了主密钥,读取旧明文数据必须透传不崩。
        let key = [99_u8; 32];
        let encrypted_repo = ChannelRepo::new(db.clone()).with_master_key(Some(key));
        let fetched = encrypted_repo
            .get(DEFAULT_OWNER_ID, created.id)
            .await
            .unwrap();
        assert_eq!(fetched.credential.expose(), "sk-legacy-plain");

        // 下次保存时自动被升级为密文。
        let saved = encrypted_repo.update(&fetched).await.unwrap();
        assert_eq!(saved.credential.expose(), "sk-legacy-plain");
        let (raw_single, _) = raw_channel_credentials(&encrypted_repo, saved.id).await;
        assert!(crate::crypto::is_encrypted(&raw_single));
    }
}
