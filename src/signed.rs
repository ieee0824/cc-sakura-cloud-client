//! Signed project transport. Private keys remain in this process, never in MCP payloads.
use anyhow::{Context, Result, bail, ensure};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use ed25519_dalek::{
    Signature, Signer, SigningKey, VerifyingKey,
    pkcs8::{DecodePrivateKey, EncodePrivateKey, EncodePublicKey},
};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{
    Deserialize, Serialize,
    de::{self, MapAccess, SeqAccess, Visitor},
};
use serde_json::{Value, json, value::RawValue};
use sha2::{Digest, Sha256};
use std::{
    fmt,
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use uuid::Uuid;

const TYPE: &str = "cc-sakura-request-v1+jws";
const MAX_COMPACT: usize = 2 * 1024 * 1024;
const MAX_PAYLOAD: usize = 1024 * 1024;

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AuthConfig {
    pub mode: String,
    pub server_id: String,
    pub project_id: String,
    pub member_id: String,
    pub private_key_file: PathBuf,
    pub outbox_file: Option<PathBuf>,
    pub recovery_key_file: Option<PathBuf>,
    #[serde(default)]
    pub allow_insecure_loopback: bool,
}
#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CompatConfig {
    #[serde(default)]
    pub room_aliases: Vec<String>,
}
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Header {
    alg: String,
    kid: String,
    typ: String,
}
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Action {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operation_id: Option<String>,
    pub op: String,
    pub project_id: String,
    pub args: Value,
}
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SignedRequest {
    pub v: u8,
    pub aud: String,
    pub member_id: String,
    pub endpoint: String,
    pub iat: i64,
    pub exp: i64,
    pub request_id: String,
    pub action: Box<RawValue>,
    #[serde(skip)]
    pub key_id: String,
}
impl SignedRequest {
    pub fn action(&self) -> Result<Action> {
        Ok(serde_json::from_str(self.action.get())?)
    }
}

pub fn now() -> Result<i64> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)?
        .as_secs()
        .try_into()?)
}
pub fn digest(bytes: &[u8]) -> String {
    format!("sha256:{}", URL_SAFE_NO_PAD.encode(Sha256::digest(bytes)))
}
pub fn key_id(public: &[u8; 32]) -> String {
    format!("ed25519:{}", URL_SAFE_NO_PAD.encode(Sha256::digest(public)))
}
fn secure_parent(path: &Path) -> Result<()> {
    let parent = path.parent().context("file needs a parent directory")?;
    if !parent.as_os_str().is_empty() && !parent.exists() {
        let mut builder = fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder.create(parent)?;
    }
    Ok(())
}
fn private_existing(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    ensure!(
        metadata.file_type().is_file(),
        "private file must be a regular file, not a symlink"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        ensure!(
            metadata.permissions().mode() & 0o077 == 0,
            "private file permissions must be 0600 or stricter"
        );
    }
    Ok(())
}
fn create_private(path: &Path) -> Result<std::fs::File> {
    secure_parent(path)?;
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    Ok(options.open(path)?)
}
pub fn generate_key(path: &Path) -> Result<Value> {
    let mut seed = [0u8; 32];
    getrandom::getrandom(&mut seed)
        .map_err(|e| anyhow::anyhow!("OS randomness unavailable: {e}"))?;
    let signing = SigningKey::from_bytes(&seed);
    seed.fill(0);
    let pem = signing.to_pkcs8_pem(Default::default())?;
    let mut file =
        create_private(path).context("create private key without overwriting existing files")?;
    file.write_all(pem.as_bytes())?;
    file.sync_all()?;
    let public = signing.verifying_key();
    Ok(
        json!({"key_id":key_id(public.as_bytes()),"public_key":URL_SAFE_NO_PAD.encode(public.as_bytes()),"public_key_pem":public.to_public_key_pem(Default::default())?}),
    )
}
pub fn save_private_profile(path: &Path, text: &str) -> Result<()> {
    let mut file = create_private(path)?;
    file.write_all(text.as_bytes())?;
    file.sync_all()?;
    Ok(())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RoomInvitation {
    pub version: u8,
    pub room_url: String,
    #[serde(rename = "token")]
    pub invitation_code: String,
}
pub fn read_invitation(path: &Path) -> Result<RoomInvitation> {
    ensure!(path.is_absolute(), "invitation_file must be absolute");
    private_existing(path)?;
    ensure!(
        fs::metadata(path)?.len() <= 8192,
        "invitation file is too large"
    );
    let bytes = fs::read(path)?;
    strict_json(&bytes).map_err(|_| anyhow::anyhow!("invalid invitation JSON"))?;
    let invitation: RoomInvitation =
        serde_json::from_slice(&bytes).map_err(|_| anyhow::anyhow!("invalid invitation fields"))?;
    ensure!(invitation.version == 1, "unsupported invitation version");
    ensure!(
        decode(&invitation.invitation_code, 32)?.len() == 32,
        "invalid invitation token"
    );
    Ok(invitation)
}

pub async fn join_agent(
    project_url: &str,
    token_file: &Path,
    private_key_file: &Path,
    allow_insecure_loopback: bool,
) -> Result<AuthConfig> {
    ensure!(token_file.is_absolute(), "token_file must be absolute");
    private_existing(token_file)?;
    ensure!(
        fs::metadata(token_file)?.len() <= 1024,
        "invitation file is too large"
    );
    let invitation = fs::read_to_string(token_file)?;
    let invitation = invitation.trim();
    join_agent_token(
        project_url,
        invitation,
        private_key_file,
        allow_insecure_loopback,
    )
    .await
}

pub async fn join_agent_token(
    project_url: &str,
    invitation: &str,
    private_key_file: &Path,
    allow_insecure_loopback: bool,
) -> Result<AuthConfig> {
    ensure!(decode(invitation, 32)?.len() == 32, "invalid invitation");
    let mut auth = AuthConfig {
        mode: "signed".into(),
        server_id: Uuid::new_v4().to_string(),
        project_id: Uuid::new_v4().to_string(),
        member_id: Uuid::new_v4().to_string(),
        private_key_file: private_key_file.into(),
        outbox_file: None,
        recovery_key_file: None,
        allow_insecure_loopback,
    };
    // Reuse the transport's canonical URL, HTTPS, and private-key file checks.
    let client = SignedClient::new(project_url, auth.clone(), CompatConfig::default())?;
    let public = URL_SAFE_NO_PAD.encode(client.key.verifying_key().as_bytes());
    let endpoint = format!("{project_url}/api/v1/join");
    let proof = serde_json::to_vec(&json!([
        "cc-sakura-agent-join-v1",
        endpoint,
        invitation,
        public
    ]))?;
    let signature = URL_SAFE_NO_PAD.encode(client.key.sign(&proof).to_bytes());
    let http = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(30))
        .build()?;
    let mut response = http
        .post(&endpoint)
        .json(&json!({"token":invitation,"public_key":public,"signature":signature}))
        .send()
        .await
        .context("room invitation request failed")?;
    ensure!(
        response.status().is_success(),
        "room invitation rejected (HTTP {})",
        response.status().as_u16()
    );
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        ensure!(
            body.len() + chunk.len() <= 65536,
            "room invitation response too large"
        );
        body.extend_from_slice(&chunk);
    }
    strict_json(&body)?;
    let result: Value = serde_json::from_slice(&body)?;
    ensure!(
        result["key_id"].as_str() == Some(&key_id(client.key.verifying_key().as_bytes())),
        "room invitation key mismatch"
    );
    for name in ["server_id", "project_id", "member_id"] {
        let value = result[name].as_str().context("missing room identity")?;
        Uuid::parse_str(value).context("invalid room identity")?;
    }
    auth.server_id = result["server_id"].as_str().unwrap().into();
    auth.project_id = result["project_id"].as_str().unwrap().into();
    auth.member_id = result["member_id"].as_str().unwrap().into();
    let enrolled = SignedClient::new(project_url, auth.clone(), CompatConfig::default())?;
    enrolled.register_encryption_device(&http).await?;
    Ok(auth)
}
fn decode(value: &str, limit: usize) -> Result<Vec<u8>> {
    ensure!(
        value.len() <= (limit * 4).div_ceil(3),
        "encoded value exceeds limit"
    );
    let decoded = URL_SAFE_NO_PAD.decode(value).context("invalid base64url")?;
    ensure!(
        decoded.len() <= limit && URL_SAFE_NO_PAD.encode(&decoded) == value,
        "noncanonical or oversized base64url"
    );
    Ok(decoded)
}
fn parts(compact: &str) -> Result<(&str, &str, &str)> {
    ensure!(compact.len() <= MAX_COMPACT, "signed request exceeds 2 MiB");
    let mut parts = compact.split('.');
    let h = parts.next().context("missing header")?;
    let p = parts.next().context("missing payload")?;
    let s = parts.next().context("missing signature")?;
    ensure!(
        parts.next().is_none() && !h.is_empty() && !p.is_empty() && !s.is_empty(),
        "invalid compact JWS"
    );
    Ok((h, p, s))
}
fn header(encoded: &str) -> Result<Header> {
    let bytes = decode(encoded, 1024)?;
    strict_json(&bytes)?;
    let header: Header = serde_json::from_slice(&bytes)?;
    ensure!(
        header.alg == "Ed25519" && header.typ == TYPE,
        "unsupported signature profile"
    );
    ensure!(
        header.kid.len() == "ed25519:".len() + 43 && header.kid.starts_with("ed25519:"),
        "invalid key ID"
    );
    Ok(header)
}
pub fn parse_and_verify(compact: &str, public_key: &str) -> Result<SignedRequest> {
    let (h, p, s) = parts(compact)?;
    let header = header(h)?;
    let public_bytes: [u8; 32] = decode(public_key, 32)?
        .try_into()
        .map_err(|_| anyhow::anyhow!("public key length"))?;
    let public = VerifyingKey::from_bytes(&public_bytes)?;
    ensure!(
        key_id(&public_bytes) == header.kid && !public.is_weak(),
        "key ID does not match public key"
    );
    let sig = Signature::from_slice(&decode(s, 64)?)?;
    public
        .verify_strict(format!("{h}.{p}").as_bytes(), &sig)
        .context("invalid request signature")?;
    let payload = decode(p, MAX_PAYLOAD)?;
    strict_json(&payload)?;
    let mut request: SignedRequest = serde_json::from_slice(&payload)?;
    ensure!(request.v == 1, "unsupported protocol version");
    canonical_uuid(&request.aud)?;
    canonical_uuid(&request.member_id)?;
    canonical_uuid(&request.request_id)?;
    let action = request.action()?;
    canonical_uuid(&action.project_id)?;
    if let Some(id) = &action.operation_id {
        canonical_uuid(id)?;
    }
    ensure!(action.args.is_object(), "action args must be an object");
    ensure!(
        action.op.len() <= 64 && !action.op.is_empty(),
        "invalid operation"
    );
    request.key_id = header.kid;
    Ok(request)
}
pub fn canonical_uuid(s: &str) -> Result<()> {
    ensure!(
        Uuid::parse_str(s)?.to_string() == s,
        "UUID must use canonical lowercase form"
    );
    Ok(())
}
pub fn valid_slug(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 63
        && s.as_bytes()[0].is_ascii_alphanumeric()
        && s.as_bytes()[s.len() - 1].is_ascii_alphanumeric()
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

// Reject duplicate object fields throughout arbitrary payload JSON, including escaped duplicates.
// Parsing to Value alone silently accepts the last duplicate and must not be an auth boundary.
struct Unique(Value);
impl<'de> Deserialize<'de> for Unique {
    fn deserialize<D: de::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = Unique;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("JSON with unique keys")
            }
            fn visit_bool<E: de::Error>(self, v: bool) -> std::result::Result<Unique, E> {
                Ok(Unique(Value::Bool(v)))
            }
            fn visit_i64<E: de::Error>(self, v: i64) -> std::result::Result<Unique, E> {
                Ok(Unique(v.into()))
            }
            fn visit_u64<E: de::Error>(self, v: u64) -> std::result::Result<Unique, E> {
                Ok(Unique(v.into()))
            }
            fn visit_f64<E: de::Error>(self, v: f64) -> std::result::Result<Unique, E> {
                serde_json::Number::from_f64(v)
                    .map(|n| Unique(n.into()))
                    .ok_or_else(|| E::custom("nonfinite number"))
            }
            fn visit_str<E: de::Error>(self, v: &str) -> std::result::Result<Unique, E> {
                Ok(Unique(v.into()))
            }
            fn visit_string<E: de::Error>(self, v: String) -> std::result::Result<Unique, E> {
                Ok(Unique(v.into()))
            }
            fn visit_none<E: de::Error>(self) -> std::result::Result<Unique, E> {
                Ok(Unique(Value::Null))
            }
            fn visit_unit<E: de::Error>(self) -> std::result::Result<Unique, E> {
                Ok(Unique(Value::Null))
            }
            fn visit_seq<A: SeqAccess<'de>>(
                self,
                mut a: A,
            ) -> std::result::Result<Unique, A::Error> {
                let mut out = Vec::new();
                while let Some(Unique(v)) = a.next_element()? {
                    out.push(v)
                }
                Ok(Unique(out.into()))
            }
            fn visit_map<A: MapAccess<'de>>(
                self,
                mut a: A,
            ) -> std::result::Result<Unique, A::Error> {
                let mut out = serde_json::Map::new();
                while let Some(key) = a.next_key::<String>()? {
                    if out.contains_key(&key) {
                        return Err(de::Error::custom("duplicate JSON key"));
                    }
                    let Unique(value) = a.next_value()?;
                    out.insert(key, value);
                }
                Ok(Unique(out.into()))
            }
        }
        d.deserialize_any(V)
    }
}
pub fn strict_json(bytes: &[u8]) -> Result<Value> {
    let Unique(value) = serde_json::from_slice(bytes)?;
    fn depth(v: &Value, n: usize) -> bool {
        n <= 64
            && match v {
                Value::Array(a) => a.iter().all(|v| depth(v, n + 1)),
                Value::Object(o) => o.values().all(|v| depth(v, n + 1)),
                _ => true,
            }
    }
    ensure!(depth(&value, 0), "JSON nesting exceeds 64");
    Ok(value)
}

fn render_codec(template: &str, payload: &Value) -> Result<String> {
    let mut output = String::new();
    let mut rest = template;
    while let Some(start) = rest.find('{') {
        let end = rest[start + 1..]
            .find('}')
            .context("invalid codec placeholder")?
            + start
            + 1;
        let value = payload
            .get(&rest[start + 1..end])
            .context("codec payload field missing")?;
        let text = match value {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        };
        ensure!(
            output.len() + start + text.len() <= 64 * 1024,
            "codec output exceeds 64 KiB"
        );
        output.push_str(&rest[..start]);
        output.push_str(&text);
        rest = &rest[end + 1..];
    }
    ensure!(
        output.len() + rest.len() <= 64 * 1024,
        "codec output exceeds 64 KiB"
    );
    output.push_str(rest);
    Ok(output)
}

#[derive(Clone)]
pub struct SignedClient {
    pub project_url: String,
    pub project_name: String,
    pub auth: AuthConfig,
    compat: CompatConfig,
    key: Arc<SigningKey>,
    outbox: PathBuf,
    room_key: Arc<std::sync::Mutex<Option<crate::e2ee::Recovery>>>,
}
impl SignedClient {
    fn recovery(&self) -> Result<Option<crate::e2ee::Recovery>> {
        let cached = self
            .room_key
            .lock()
            .map_err(|_| anyhow::anyhow!("room key state unavailable"))?
            .clone();
        if cached.is_some() {
            return Ok(cached);
        }
        self.auth
            .recovery_key_file
            .as_ref()
            .map(|path| {
                ensure!(path.is_absolute(), "recovery_key_file must be absolute");
                private_existing(path)?;
                let text = fs::read_to_string(path)?;
                ensure!(text.len() <= 4 * 1024 * 1024, "recovery file is too large");
                crate::e2ee::Recovery::parse(&text, &self.auth.server_id, &self.auth.project_id)
            })
            .transpose()
    }
    fn encryption_device(&self) -> Result<crate::key_delivery::Device> {
        let path = self.auth.private_key_file.with_extension("delivery.json");
        if !path.exists() {
            let device = crate::key_delivery::Device::generate(
                &self.auth.server_id,
                &self.auth.project_id,
                &self.auth.member_id,
            )?;
            match save_private_profile(&path, &serde_json::to_string(&device)?) {
                Ok(()) => {}
                Err(e) if path.exists() => drop(e),
                Err(e) => return Err(e),
            }
        }
        private_existing(&path)?;
        ensure!(
            fs::metadata(&path)?.len() <= 16384,
            "delivery device file too large"
        );
        crate::key_delivery::Device::parse(
            &fs::read_to_string(path)?,
            &self.auth.server_id,
            &self.auth.project_id,
            &self.auth.member_id,
        )
    }
    async fn register_encryption_device(
        &self,
        http: &reqwest::Client,
    ) -> Result<crate::key_delivery::Device> {
        let device = self.encryption_device()?;
        let result = self
            .send_action(
                http,
                &self.action(
                    "encryption-device",
                    json!({"public_key":device.public_key()?}),
                    None,
                )?,
            )
            .await?;
        ensure!(
            result["device_id"].as_str() == Some(&device.id()?),
            "registered delivery key mismatch"
        );
        Ok(device)
    }
    async fn sync_room_key(&self, http: &reqwest::Client, key_id: &str) -> Result<()> {
        crate::timing::measure(
            "e2ee.key_sync",
            "encryption-key",
            self.sync_room_key_inner(http, key_id),
        )
        .await
    }
    async fn sync_room_key_inner(&self, http: &reqwest::Client, key_id: &str) -> Result<()> {
        if self
            .room_key
            .lock()
            .map_err(|_| anyhow::anyhow!("room key state unavailable"))?
            .as_ref()
            .is_some_and(|r| r.key_id == key_id)
        {
            return Ok(());
        }
        let existing = self.recovery()?;
        let mut recovery = if let Some(key) = existing.filter(|r| r.key_id == key_id) {
            key
        } else {
            let device = self.register_encryption_device(http).await?;
            let delivery = self
                .send_action(http, &self.action("encryption-key", json!({}), None)?)
                .await?;
            ensure!(
                delivery["key_id"].as_str() == Some(key_id) && delivery["envelope"].is_object(),
                "room key delivery pending; keep the owner's room page open and retry"
            );
            device.unwrap(&delivery["envelope"], key_id)?
        };
        let history = self
            .send_action(http, &self.action("encryption-history", json!({}), None)?)
            .await?;
        recovery.restore_history(
            history["history"]
                .as_array()
                .context("missing encrypted key history")?,
        )?;
        *self
            .room_key
            .lock()
            .map_err(|_| anyhow::anyhow!("room key state unavailable"))? = Some(recovery);
        Ok(())
    }
    pub fn new(project_url: &str, auth: AuthConfig, compat: CompatConfig) -> Result<Self> {
        ensure!(auth.mode == "signed", "auth.mode must be signed");
        canonical_uuid(&auth.server_id)?;
        canonical_uuid(&auth.project_id)?;
        canonical_uuid(&auth.member_id)?;
        let url = reqwest::Url::parse(project_url).context("invalid project_url")?;
        ensure!(
            url.username().is_empty()
                && url.password().is_none()
                && url.query().is_none()
                && url.fragment().is_none(),
            "project_url cannot contain credentials, query or fragment"
        );
        let host = url.host_str().context("project_url needs a host")?;
        let loopback = host == "localhost"
            || host
                .parse::<std::net::IpAddr>()
                .is_ok_and(|ip| ip.is_loopback());
        ensure!(
            url.scheme() == "https"
                || (auth.allow_insecure_loopback && url.scheme() == "http" && loopback),
            "signed transport requires HTTPS (test HTTP must be explicitly enabled on loopback)"
        );
        let path = url
            .path()
            .strip_prefix('/')
            .context("invalid project path")?;
        let segments: Vec<_> = path.split('/').collect();
        ensure!(
            segments.len() == 2 && segments.iter().all(|s| valid_slug(s)),
            "project_url must end in /owner/project using canonical slugs"
        );
        ensure!(
            url.as_str() == project_url,
            "project_url must use its canonical form"
        );
        ensure!(
            auth.private_key_file.is_absolute(),
            "private_key_file must be absolute"
        );
        private_existing(&auth.private_key_file)?;
        let key = SigningKey::from_pkcs8_pem(&fs::read_to_string(&auth.private_key_file)?)
            .context("invalid Ed25519 PKCS#8 private key")?;
        let outbox = match &auth.outbox_file {
            Some(path) => path.clone(),
            None => std::env::var_os("XDG_STATE_HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| {
                    PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".local/state")
                })
                .join("cc-sakura/outbox.sqlite3"),
        };
        ensure!(outbox.is_absolute(), "outbox_file must be absolute");
        Ok(Self {
            project_url: project_url.into(),
            project_name: path.into(),
            auth,
            compat,
            key: Arc::new(key),
            outbox,
            room_key: Arc::new(std::sync::Mutex::new(None)),
        })
    }
    fn outbox(&self) -> Result<Connection> {
        if !self.outbox.exists() {
            match create_private(&self.outbox) {
                Ok(file) => file.sync_all()?,
                Err(e) if self.outbox.exists() => drop(e),
                Err(e) => return Err(e),
            }
        }
        private_existing(&self.outbox)?;
        let db = Connection::open(&self.outbox)?;
        db.busy_timeout(Duration::from_secs(5))?;
        db.execute_batch("PRAGMA journal_mode=DELETE;CREATE TABLE IF NOT EXISTS signed_outbox(id TEXT PRIMARY KEY,server_id TEXT NOT NULL,project_id TEXT NOT NULL,member_id TEXT NOT NULL,action TEXT NOT NULL,result TEXT,created_at INTEGER NOT NULL);")?;
        Ok(db)
    }
    pub fn target(&self, args: &Value) -> Result<Value> {
        let mut args = args
            .as_object()
            .context("arguments must be an object")?
            .clone();
        let room = args.remove("room");
        let project = args.remove("project");
        ensure!(
            room.is_none() || project.is_none(),
            "specify project or legacy room alias, not both"
        );
        if let Some(project) = project {
            ensure!(
                project.as_str() == Some(&self.project_name),
                "project does not match configured project"
            );
        }
        if let Some(room) = room {
            let room = room.as_str().context("room must be a string")?;
            ensure!(
                room == self.project_name || self.compat.room_aliases.iter().any(|a| a == room),
                "room must match the configured owner/project or an explicit compat.room_aliases entry"
            );
        }
        Ok(args.into())
    }
    fn action(&self, op: &str, args: Value, operation_id: Option<String>) -> Result<Box<RawValue>> {
        Ok(RawValue::from_string(serde_json::to_string(&Action {
            op: op.into(),
            project_id: self.auth.project_id.clone(),
            operation_id,
            args,
        })?)?)
    }
    pub fn sign_action(&self, action: &RawValue, endpoint: &str) -> Result<String> {
        self.sign_action_at(action, endpoint, now()?, Uuid::new_v4().to_string())
    }
    fn sign_action_at(
        &self,
        action: &RawValue,
        endpoint: &str,
        iat: i64,
        request_id: String,
    ) -> Result<String> {
        let request = SignedRequest {
            v: 1,
            aud: self.auth.server_id.clone(),
            member_id: self.auth.member_id.clone(),
            endpoint: endpoint.into(),
            iat,
            exp: iat + 120,
            request_id,
            action: action.to_owned(),
            key_id: String::new(),
        };
        let header = Header {
            alg: "Ed25519".into(),
            kid: key_id(self.key.verifying_key().as_bytes()),
            typ: TYPE.into(),
        };
        let payload = serde_json::to_vec(&request)?;
        ensure!(
            payload.len() <= MAX_PAYLOAD,
            "signed request payload exceeds 1 MiB"
        );
        let input = format!(
            "{}.{}",
            URL_SAFE_NO_PAD.encode(serde_json::to_vec(&header)?),
            URL_SAFE_NO_PAD.encode(payload)
        );
        let sig = self.key.sign(input.as_bytes());
        Ok(format!(
            "{input}.{}",
            URL_SAFE_NO_PAD.encode(sig.to_bytes())
        ))
    }
    async fn request(
        &self,
        http: &reqwest::Client,
        action: &RawValue,
        events: bool,
    ) -> Result<reqwest::Response> {
        let suffix = if events { "events" } else { "rpc" };
        let endpoint = format!("/{}/api/v1/{suffix}", self.project_name);
        let op = serde_json::from_str::<Action>(action.get())?.op;
        let body = crate::timing::measure_sync("request.sign", &op, || {
            self.sign_action(action, &endpoint)
        })?;
        let mut request = http
            .post(format!("{}{}/{}", self.project_url, "/api/v1", suffix))
            .header("Content-Type", "application/jose")
            .body(body);
        if events {
            request = request.header("Accept", "text/event-stream");
        } else if serde_json::from_str::<Action>(action.get())?.op == "receive" {
            request = request.timeout(Duration::from_secs(330));
        }
        crate::timing::measure(
            if events {
                "http.sse_headers"
            } else if op == "receive" {
                "http.long_poll_headers"
            } else {
                "http.headers"
            },
            &op,
            async {
                request
                    .send()
                    .await
                    .context("connect to signed sakura project")
            },
        )
        .await
    }
    async fn response(response: reqwest::Response) -> Result<Value> {
        let status = response.status();
        if status.is_server_error() {
            response.error_for_status_ref()?;
        }
        let mut response = response;
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await? {
            ensure!(
                bytes.len() + chunk.len() <= 8 * 1024 * 1024,
                "response exceeds 8 MiB"
            );
            bytes.extend_from_slice(&chunk);
        }
        let value: Value =
            serde_json::from_slice(&bytes).context("invalid signed server response")?;
        if !status.is_success() {
            bail!(
                "{}: {}",
                value["code"].as_str().unwrap_or("request_failed"),
                value["error"].as_str().unwrap_or("signed request failed")
            );
        }
        Ok(value)
    }
    async fn send_action(&self, http: &reqwest::Client, action: &RawValue) -> Result<Value> {
        let op = serde_json::from_str::<Action>(action.get())?.op;
        let response = self.request(http, action, false).await?;
        crate::timing::measure("http.body_and_json", &op, Self::response(response)).await
    }
    fn decode_legacy_message(&self, message: &mut Value) -> Result<()> {
        // Encryption authenticates ciphertext, not the original historical author.
        message["auth"]["verified"] = json!(false);
        message["auth"]["server_authenticated"] = json!(false);
        message["auth"]["decrypted"] = json!(false);
        self.decrypt_history_message(message)
    }
    fn decrypt_history_message(&self, message: &mut Value) -> Result<()> {
        if message["payload"]["type"] == "e2ee-v1" {
            if let Some(recovery) = self.recovery()? {
                let clear = crate::timing::measure_sync("e2ee.decrypt", "history", || {
                    recovery.decrypt(
                        message["sender"].as_str().context("missing sender")?,
                        &message["payload"],
                    )
                })?;
                message["encrypted_payload"] = message["payload"].clone();
                message["payload"] = clear;
                message["human"] = Value::Null;
                message["auth"]["decrypted"] = json!(true);
            }
        }
        Ok(())
    }
    pub async fn verify_history(&self, http: &reqwest::Client, value: Value) -> Result<Value> {
        crate::timing::measure(
            "history.verify_and_decrypt",
            "history",
            self.verify_history_inner(http, value),
        )
        .await
    }
    async fn verify_history_inner(
        &self,
        http: &reqwest::Client,
        mut value: Value,
    ) -> Result<Value> {
        if value["messages"]
            .as_array()
            .is_some_and(|m| m.iter().any(|v| v["payload"]["type"] == "e2ee-v1"))
        {
            if let Some(id) = value["encryption_key_id"].as_str() {
                self.sync_room_key(http, id).await?;
            }
        }
        let Some(messages) = value.get("messages").and_then(Value::as_array) else {
            return Ok(value);
        };
        if messages.is_empty() {
            return Ok(value);
        }
        let codec_names: std::collections::BTreeSet<_> = messages
            .iter()
            .filter_map(|message| message["codec"].as_str())
            .collect();
        let mut codecs = value
            .get("codecs")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let missing: Vec<_> = codec_names
            .into_iter()
            .filter(|name| !codecs.iter().any(|codec| codec["name"] == *name))
            .collect();
        for names in missing.chunks(100) {
            let lookup = self.action("codecs", json!({"names":names}), None)?;
            codecs.extend(
                self.send_action(http, &lookup).await?["codecs"]
                    .as_array()
                    .context("missing codec definitions")?
                    .iter()
                    .cloned(),
            );
        }
        let lookup = self.action("keys", json!({}), None)?;
        let key_response = self.send_action(http, &lookup).await?;
        let keys = key_response["keys"]
            .as_array()
            .context("missing project public keys")?;
        for message in value["messages"].as_array_mut().unwrap() {
            let auth = message
                .get("auth")
                .context("missing message authentication metadata")?;
            let scheme = auth["scheme"]
                .as_str()
                .context("missing message authentication scheme")?;
            if scheme == "legacy" {
                self.decode_legacy_message(message)?;
                continue;
            }
            let key_id = auth["key_id"].as_str().context("missing message key ID")?;
            let key = keys
                .iter()
                .find(|k| k["key_id"] == key_id)
                .context("message key is not registered in this project")?;
            let public_key = key["public_key"].as_str().context("missing public key")?;
            let (action_raw, member_id, server_id) = match scheme {
                "signed-v1" => {
                    ensure!(key["kind"] == "ed25519", "signature/key type mismatch");
                    let proof = auth["proof_jws"].as_str().context("missing JWS proof")?;
                    let request = parse_and_verify(proof, public_key)?;
                    ensure!(request.key_id == key_id, "proof key ID mismatch");
                    (request.action, request.member_id, request.aud)
                }
                "webauthn-v1" => {
                    ensure!(key["kind"] == "webauthn", "signature/key type mismatch");
                    let proof = auth.get("proof").context("missing WebAuthn proof")?;
                    let context = crate::browser_proof::verify_proof(proof, public_key)?;
                    let claims = strict_json(
                        proof["statement"]
                            .as_str()
                            .context("missing WebAuthn statement")?
                            .as_bytes(),
                    )?;
                    ensure!(
                        claims["user_id"] == key["user_id"],
                        "proof user differs from registered key owner"
                    );
                    let raw = context["action_json"]
                        .as_str()
                        .context("missing signed action")?;
                    strict_json(raw.as_bytes())?;
                    (
                        RawValue::from_string(raw.into())?,
                        context["member_id"]
                            .as_str()
                            .context("missing signed member")?
                            .into(),
                        context["server_id"]
                            .as_str()
                            .context("missing signed server")?
                            .into(),
                    )
                }
                "session-v1" => {
                    // A hosted browser post is authenticated by the server's
                    // session checks. The receipt is not a user's signature.
                    ensure!(key["kind"] == "webauthn", "session key type mismatch");
                    let receipt = auth.get("receipt").context("missing session receipt")?;
                    ensure!(
                        receipt["key_id"] == key_id && receipt["user_id"] == key["user_id"],
                        "session receipt owner mismatch"
                    );
                    ensure!(
                        receipt["project_id"] == self.auth.project_id,
                        "session receipt project mismatch"
                    );
                    let raw = receipt["action_json"]
                        .as_str()
                        .context("missing session action")?;
                    strict_json(raw.as_bytes())?;
                    (
                        RawValue::from_string(raw.into())?,
                        receipt["member_id"]
                            .as_str()
                            .context("missing session member")?
                            .into(),
                        receipt["server_id"]
                            .as_str()
                            .context("missing session server")?
                            .into(),
                    )
                }
                _ => bail!("unsupported message signature scheme"),
            };
            let action: Action = serde_json::from_str(action_raw.get())?;
            ensure!(
                key["member_id"] == member_id,
                "proof member differs from registered key binding"
            );
            ensure!(
                server_id == self.auth.server_id
                    && action.project_id == self.auth.project_id
                    && action.op == "send",
                "message proof has the wrong server, project or operation"
            );
            ensure!(
                message["sender"] == member_id && message["payload"] == action.args["payload"],
                "message display differs from signed author or payload"
            );
            ensure!(
                auth["action_digest"] == digest(action_raw.get().as_bytes()),
                "message action digest mismatch"
            );
            let mentions = |value: &Value| -> Result<std::collections::BTreeSet<String>> {
                match value {
                    Value::Null => Ok(Default::default()),
                    Value::Array(values) => values
                        .iter()
                        .map(|v| Ok(v.as_str().context("invalid mention")?.to_owned()))
                        .collect(),
                    _ => bail!("invalid mentions"),
                }
            };
            ensure!(
                mentions(&message["mentions"])? == mentions(&action.args["mentions"])?,
                "message mentions differ from signed mentions"
            );
            ensure!(
                message["codec"] == action.args["codec"],
                "message codec differs from signed codec"
            );
            if let Some(codec) = action.args["codec"].as_str() {
                let template = codecs
                    .iter()
                    .find(|c| c["name"] == codec)
                    .and_then(|c| c["template"].as_str())
                    .context("missing signed codec definition")?;
                ensure!(
                    action.args["codec_digest"] == digest(template.as_bytes()),
                    "codec definition differs from signed digest"
                );
                if let Some(human) = message.get("human").and_then(Value::as_str) {
                    ensure!(
                        render_codec(template, &action.args["payload"])? == human,
                        "rendered human text differs from signed codec"
                    );
                }
            } else if let Some(human) = message.get("human").and_then(Value::as_str) {
                ensure!(
                    action.args["gloss"].as_str().unwrap_or("") == human,
                    "message gloss differs from signed gloss"
                );
            }
            let session_authenticated = scheme == "session-v1";
            message["auth"]["verified"] = json!(!session_authenticated);
            message["auth"]["server_authenticated"] = json!(session_authenticated);
            self.decrypt_history_message(message)?;
        }
        Ok(value)
    }
    pub async fn call(
        &self,
        http: &reqwest::Client,
        operation: &str,
        args: Value,
    ) -> Result<Value> {
        crate::timing::measure(
            "operation",
            operation,
            self.call_inner(http, operation, args),
        )
        .await
    }
    async fn call_inner(
        &self,
        http: &reqwest::Client,
        operation: &str,
        args: Value,
    ) -> Result<Value> {
        let mut args = self.target(&args)?;
        let requested_id = args.get("id").cloned();
        let requested_handle = args.get("handle").cloned();
        let op = if operation == "join" {
            "identity"
        } else {
            operation
        };
        if operation == "join" {
            if let Some(id) = &requested_id {
                ensure!(
                    id.as_str() == Some(&self.auth.member_id),
                    "join UUID does not match configured member"
                );
            }
            args = json!({});
        }
        if matches!(op, "send" | "codec") {
            let identity = self
                .send_action(http, &self.action("identity", json!({}), None)?)
                .await?;
            ensure!(
                identity["encryption_rotation_required"]
                    .as_i64()
                    .unwrap_or(0)
                    == 0,
                "room key rotation pending; keep the owner's room page open and retry"
            );
            if let Some(key_id) = identity["encryption_key_id"].as_str() {
                self.sync_room_key(http, key_id).await?;
                let recovery = self.recovery()?;
                ensure!(
                    recovery.as_ref().is_some_and(|r| r.key_id == key_id),
                    "room key is not available"
                );
            }
            let recovery = self.recovery()?;
            if recovery.is_some() {
                ensure!(
                    op == "send"
                        && args["codec"].is_null()
                        && args["codec_digest"].is_null()
                        && args["gloss"].is_null(),
                    "encrypted codecs and gloss are not supported"
                );
                ensure!(
                    !matches!(
                        args["payload"]["type"].as_str(),
                        Some("approval_request" | "approval_decision")
                    ),
                    "encrypted approvals are not supported yet"
                );
            }
        }
        if op == "send"
            && args["payload"]["type"] == "approval_decision"
            && args["payload"].get("request_digest").is_none()
        {
            let id = args["payload"]["reply_to"]
                .as_i64()
                .context("approval reply_to must be one message ID")?;
            ensure!(id > 0, "invalid approval request ID");
            let lookup = self.action(
                "history",
                json!({"after":id-1,"before":id+1,"limit":1}),
                None,
            )?;
            let history = self
                .verify_history(http, self.send_action(http, &lookup).await?)
                .await?;
            let request = history["messages"]
                .as_array()
                .and_then(|ms| ms.iter().find(|m| m["id"] == id))
                .context("approval request not found")?;
            ensure!(
                request["auth"]["verified"] == true,
                "approval requires a signed request"
            );
            args["payload"]["request_digest"] = request["auth"]["action_digest"].clone();
        }
        if op == "send"
            && let Some(codec) = args.get("codec").and_then(Value::as_str)
        {
            let lookup = self.action("codecs", json!({"name":codec}), None)?;
            let codecs = self.send_action(http, &lookup).await?;
            let template = codecs["codecs"]
                .as_array()
                .and_then(|items| items.iter().find(|c| c["name"] == codec))
                .and_then(|c| c["template"].as_str())
                .context("codec not returned by project")?;
            args["codec_digest"] = json!(digest(template.as_bytes()));
        }
        let operation_id = matches!(op, "send" | "codec").then(|| Uuid::new_v4().to_string());
        if let Some(recovery) = self.recovery()? {
            ensure!(op != "codec", "codecs are not supported with encryption");
            if op == "send" {
                ensure!(
                    args["codec"].is_null()
                        && args["codec_digest"].is_null()
                        && args["gloss"].is_null(),
                    "encrypted posts cannot contain codec or gloss"
                );
                args["payload"] = crate::timing::measure_sync("e2ee.encrypt", "send", || {
                    recovery.encrypt(
                        &self.auth.member_id,
                        operation_id.as_deref().unwrap(),
                        &args["payload"],
                    )
                })?;
            }
        }
        let action = self.action(op, args, operation_id.clone())?;
        if let Some(id) = &operation_id {
            self.outbox()?.execute("INSERT INTO signed_outbox(id,server_id,project_id,member_id,action,created_at) VALUES(?1,?2,?3,?4,?5,?6)",params![id,self.auth.server_id,self.auth.project_id,self.auth.member_id,action.get(),now()?])?;
        }
        let result = self.send_action(http, &action).await;
        let value=match (result,&operation_id) {
            (Ok(value),Some(id))=>{self.complete(id,&value).with_context(||format!("operation {id} succeeded but its local receipt could not be saved; retry this operation ID"))?;value},
            (Ok(value),None)=>value,
            (Err(e),Some(id))=>return Err(e.context(format!("operation {id} is saved in the outbox; retry this operation ID instead of sending again"))),
            (Err(e),None)=>return Err(e),
        };
        if operation == "join"
            && let Some(handle) = requested_handle
        {
            ensure!(
                value["handle"] == handle,
                "join handle does not match registered member"
            );
        }
        self.verify_history(http, value).await
    }
    fn complete(&self, id: &str, value: &Value) -> Result<()> {
        self.outbox()?.execute(
            "UPDATE signed_outbox SET result=?1 WHERE id=?2",
            params![value.to_string(), id],
        )?;
        Ok(())
    }
    pub async fn retry(&self, http: &reqwest::Client, id: &str) -> Result<Value> {
        crate::timing::measure("operation", "retry", self.retry_inner(http, id)).await
    }
    async fn retry_inner(&self, http: &reqwest::Client, id: &str) -> Result<Value> {
        canonical_uuid(id)?;
        let row:Option<(String,Option<String>)>=self.outbox()?.query_row("SELECT action,result FROM signed_outbox WHERE id=?1 AND server_id=?2 AND project_id=?3 AND member_id=?4",params![id,self.auth.server_id,self.auth.project_id,self.auth.member_id],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
        let (action, result) = row.context("operation is not in this project/member outbox")?;
        if let Some(result) = result {
            return Ok(serde_json::from_str(&result)?);
        }
        let action = RawValue::from_string(action)?;
        let parsed: Action = serde_json::from_str(action.get())?;
        let identity = self
            .send_action(http, &self.action("identity", json!({}), None)?)
            .await?;
        if identity["encryption_key_id"].is_string() || self.auth.recovery_key_file.is_some() {
            ensure!(
                parsed.args["payload"]["type"] == "e2ee-v1",
                "refusing to resend an unencrypted pending operation"
            );
        }
        let result = self.send_action(http, &action).await?;
        self.complete(id, &result).with_context(|| {
            format!("operation {id} succeeded but its local receipt could not be saved")
        })?;
        Ok(result)
    }
    pub async fn events(&self, http: &reqwest::Client, args: &Value) -> Result<reqwest::Response> {
        crate::timing::measure("sse.open", "subscribe", self.events_inner(http, args)).await
    }
    async fn events_inner(
        &self,
        http: &reqwest::Client,
        args: &Value,
    ) -> Result<reqwest::Response> {
        let action = self.action("subscribe", self.target(args)?, None)?;
        let response = self.request(http, &action, true).await?;
        response.error_for_status_ref()?;
        ensure!(
            response
                .headers()
                .get("content-type")
                .is_some_and(|v| v.to_str().unwrap_or("").starts_with("text/event-stream")),
            "signed server did not return an event stream"
        );
        Ok(response)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn encrypted_legacy_history_decrypts_without_authenticating_its_author() -> Result<()> {
        let fixture: Value = serde_json::from_str(include_str!("../tests/fixtures/e2ee-v1.json"))?;
        let r = &fixture["recovery"];
        let recovery = crate::e2ee::Recovery::parse(
            &r.to_string(),
            r["server_id"].as_str().unwrap(),
            r["project_id"].as_str().unwrap(),
        )?;
        let dir = tempfile::tempdir()?;
        let c = client(dir.path())?;
        *c.room_key.lock().unwrap() = Some(recovery);
        let original = json!({"sender":fixture["member"],"payload":fixture["payload"],"auth":{"scheme":"legacy","verified":true,"server_authenticated":true},"human":"untrusted gloss"});
        let mut message = original.clone();
        c.decode_legacy_message(&mut message)?;
        assert_eq!(message["payload"], fixture["clear"]);
        assert_eq!(message["encrypted_payload"], fixture["payload"]);
        assert_eq!(message["auth"]["verified"], false);
        assert_eq!(message["auth"]["server_authenticated"], false);
        assert_eq!(message["auth"]["decrypted"], true);
        assert!(message["human"].is_null());
        let mut tampered = original.clone();
        tampered["sender"] = json!("another sender");
        assert!(c.decode_legacy_message(&mut tampered).is_err());
        let mut tampered = original;
        tampered["payload"]["operation_id"] = json!("changed");
        assert!(c.decode_legacy_message(&mut tampered).is_err());
        let mut plain = json!({"payload":{"message":"old plaintext"},"auth":{"scheme":"legacy"}});
        c.decode_legacy_message(&mut plain)?;
        assert_eq!(plain["payload"]["message"], "old plaintext");
        assert_eq!(plain["auth"]["verified"], false);
        assert_eq!(plain["auth"]["decrypted"], false);
        Ok(())
    }
    #[tokio::test]
    async fn agent_waits_for_delivery_then_encrypts_before_outbox_and_reloads_after_restart()
    -> Result<()> {
        use axum::{Json, Router, routing::post};
        use std::sync::atomic::{AtomicBool, Ordering};
        let dir = tempfile::tempdir()?;
        let initial = client(dir.path())?;
        let device = initial.encryption_device()?;
        let wrapped = crate::key_delivery::tests::browser(
            json!({"server":initial.auth.server_id,"project":initial.auth.project_id,"member":initial.auth.member_id,"device":{"id":device.id()?,"public_key":device.public_key()?}}),
        )?;
        let bundle = wrapped["bundle"].clone();
        let ready = Arc::new(AtomicBool::new(false));
        let server_ready = ready.clone();
        let expected = initial.key.verifying_key();
        let device_id = device.id()?;
        let server = initial.auth.server_id.clone();
        let project = initial.auth.project_id.clone();
        let member = initial.auth.member_id.clone();
        let server_member = member.clone();
        let server_bundle = bundle.clone();
        let app = Router::new().route(
            "/owner/project/api/v1/rpc",
            post(move |body: String| {
                let ready = server_ready.clone();
                let wrapped = wrapped.clone();
                let device_id = device_id.clone();
                let bundle = server_bundle.clone();
                let server = server.clone();
                let project = project.clone();
                let member = server_member.clone();
                async move {
                    let verified =
                        parse_and_verify(&body, &URL_SAFE_NO_PAD.encode(expected.as_bytes()))
                            .unwrap();
                    let a = verified.action().unwrap();
                    let result = match a.op.as_str() {
                        "identity" => json!({"encryption_key_id":bundle["key_id"]}),
                        "encryption-device" => json!({"device_id":device_id}),
                        "encryption-history" => json!({"history":[]}),
                        "encryption-key" => {
                            if ready.load(Ordering::SeqCst) {
                                json!({"key_id":bundle["key_id"],"envelope":wrapped["envelope"]})
                            } else {
                                json!({"key_id":null,"envelope":null})
                            }
                        }
                        "send" => {
                            assert!(!a.args["payload"].to_string().contains("secret agent draft"));
                            let recovery = crate::e2ee::Recovery::parse(
                                &bundle.to_string(),
                                &server,
                                &project,
                            )
                            .unwrap();
                            assert_eq!(
                                recovery.decrypt(&member, &a.args["payload"]).unwrap()["message"],
                                "secret agent draft"
                            );
                            json!({"id":1})
                        }
                        _ => panic!("unexpected operation"),
                    };
                    Json(result)
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let url = format!("http://{}/owner/project", listener.local_addr()?);
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let mut auth = initial.auth.clone();
        auth.allow_insecure_loopback = true;
        let c = SignedClient::new(&url, auth.clone(), CompatConfig::default())?;
        let http = reqwest::Client::new();
        let args = json!({"payload":{"message":"secret agent draft"}});
        let error = c.call(&http, "send", args.clone()).await.unwrap_err();
        assert!(error.to_string().contains("delivery pending"));
        assert!(!c.outbox.exists());
        ready.store(true, Ordering::SeqCst);
        assert_eq!(c.call(&http, "send", args.clone()).await?["id"], 1);
        let action: String =
            c.outbox()?
                .query_row("SELECT action FROM signed_outbox LIMIT 1", [], |r| r.get(0))?;
        assert!(!action.contains("secret agent draft"));
        let persisted =
            fs::read_to_string(c.auth.private_key_file.with_extension("delivery.json"))?;
        assert!(!persisted.contains(bundle["key"].as_str().unwrap()));
        let restarted = SignedClient::new(&url, auth, CompatConfig::default())?;
        assert_eq!(restarted.call(&http, "send", args).await?["id"], 1);
        task.abort();
        Ok(())
    }
    #[tokio::test]
    async fn room_invitation_registers_proof_of_possession_and_returns_a_usable_profile()
    -> Result<()> {
        use axum::{Json, Router, routing::post};
        let dir = tempfile::tempdir()?;
        let c = client(dir.path())?;
        let invitation = URL_SAFE_NO_PAD.encode([7u8; 32]);
        let token_path = dir.path().join("invitation.txt");
        save_private_profile(&token_path, &invitation)?;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let project_url = format!("http://{}/owner/project", listener.local_addr()?);
        let endpoint = format!("{project_url}/api/v1/join");
        let expected = c.key.verifying_key();
        let identity = json!({"server_id":Uuid::new_v4(),"project_id":Uuid::new_v4(),"member_id":Uuid::new_v4(),"key_id":key_id(expected.as_bytes())});
        let reply = identity.clone();
        let app = Router::new().route(
            "/owner/project/api/v1/join",
            post(move |Json(body): Json<Value>| {
                let endpoint = endpoint.clone();
                let identity = identity.clone();
                let invitation = invitation.clone();
                async move {
                    assert_eq!(body["token"], invitation);
                    assert_eq!(
                        body["public_key"],
                        URL_SAFE_NO_PAD.encode(expected.as_bytes())
                    );
                    let proof = serde_json::to_vec(&json!([
                        "cc-sakura-agent-join-v1",
                        endpoint,
                        body["token"],
                        body["public_key"]
                    ]))
                    .unwrap();
                    let signature = Signature::from_slice(
                        &URL_SAFE_NO_PAD
                            .decode(body["signature"].as_str().unwrap())
                            .unwrap(),
                    )
                    .unwrap();
                    expected.verify_strict(&proof, &signature).unwrap();
                    Json(identity)
                }
            }),
        );
        let app = app.route(
            "/owner/project/api/v1/rpc",
            post(move |body: String| async move {
                let request =
                    parse_and_verify(&body, &URL_SAFE_NO_PAD.encode(expected.as_bytes())).unwrap();
                let action = request.action().unwrap();
                assert_eq!(action.op, "encryption-device");
                let p = &action.args["public_key"];
                let id = URL_SAFE_NO_PAD.encode(Sha256::digest(
                    serde_json::to_vec(&json!([
                        crate::key_delivery::SUITE,
                        p["p256"]["x"],
                        p["p256"]["y"],
                        p["ml_kem"]
                    ]))
                    .unwrap(),
                ));
                Json(json!({"device_id":id}))
            }),
        );
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let auth = join_agent(&project_url, &token_path, &c.auth.private_key_file, true).await?;
        assert_eq!(auth.member_id, reply["member_id"].as_str().unwrap());
        assert_eq!(auth.project_id, reply["project_id"].as_str().unwrap());
        let joined = SignedClient::new(&project_url, auth, CompatConfig::default())?;
        let action = RawValue::from_string(
            json!({"project_id":joined.auth.project_id,"op":"identity","args":{}}).to_string(),
        )?;
        let compact = joined.sign_action(&action, "/owner/project/api/v1/rpc")?;
        assert_eq!(
            parse_and_verify(&compact, &URL_SAFE_NO_PAD.encode(expected.as_bytes()))?.member_id,
            joined.auth.member_id
        );
        let json_path = dir.path().join("invitation.json");
        save_private_profile(
            &json_path,
            &json!({"version":1,"room_url":project_url,"token":fs::read_to_string(&token_path)?})
                .to_string(),
        )?;
        let packaged = read_invitation(&json_path)?;
        let packaged_auth = join_agent_token(
            &packaged.room_url,
            &packaged.invitation_code,
            &c.auth.private_key_file,
            true,
        )
        .await?;
        assert_eq!(packaged_auth.member_id, joined.auth.member_id);
        for url in [
            "https://example.com/owner/project?token=secret",
            "https://user:pass@example.com/owner/project",
            "https://example.com/owner/project/",
            "http://example.com/owner/project",
        ] {
            assert!(
                join_agent_token(
                    url,
                    &packaged.invitation_code,
                    &c.auth.private_key_file,
                    false
                )
                .await
                .is_err()
            );
        }
        task.abort();
        assert!(
            join_agent(&project_url, &token_path, &c.auth.private_key_file, false)
                .await
                .is_err()
        );
        let profile = dir.path().join("agent.toml");
        save_private_profile(&profile, "test")?;
        assert!(save_private_profile(&profile, "overwrite").is_err());
        private_existing(&profile)?;
        Ok(())
    }
    fn client(dir: &Path) -> Result<SignedClient> {
        let file = dir.join("key.pem");
        generate_key(&file)?;
        SignedClient::new(
            "https://example.org/owner/project",
            AuthConfig {
                mode: "signed".into(),
                server_id: Uuid::new_v4().to_string(),
                project_id: Uuid::new_v4().to_string(),
                member_id: Uuid::new_v4().to_string(),
                private_key_file: file,
                outbox_file: Some(dir.join("outbox.sqlite3")),
                recovery_key_file: None,
                allow_insecure_loopback: false,
            },
            CompatConfig {
                room_aliases: vec!["old-room".into()],
            },
        )
    }
    #[test]
    fn signed_request_retains_raw_action_and_binds_every_field() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let client = client(dir.path())?;
        let action = RawValue::from_string(format!(
            "{{\"op\":\"send\", \"project_id\":\"{}\",\"operation_id\":\"{}\",\"args\":{{\"payload\":{{\"x\":1}},\"mentions\":[]}}}}",
            client.auth.project_id,
            Uuid::new_v4()
        ))?;
        let compact = client.sign_action_at(
            &action,
            "/owner/project/api/v1/rpc",
            1000,
            Uuid::new_v4().to_string(),
        )?;
        let public = URL_SAFE_NO_PAD.encode(client.key.verifying_key().as_bytes());
        let verified = parse_and_verify(&compact, &public)?;
        assert_eq!(verified.action.get(), action.get());
        for field in [
            "aud",
            "member_id",
            "endpoint",
            "iat",
            "exp",
            "request_id",
            "action",
        ] {
            let (h, p, s) = parts(&compact)?;
            let mut value: Value = serde_json::from_slice(&decode(p, MAX_PAYLOAD)?)?;
            value[field] = json!("changed");
            let altered = format!("{h}.{}.{s}", URL_SAFE_NO_PAD.encode(value.to_string()));
            assert!(parse_and_verify(&altered, &public).is_err(), "{field}");
        }
        Ok(())
    }
    #[test]
    fn rejects_duplicate_keys_and_algorithm_confusion() -> Result<()> {
        assert!(strict_json(br#"{"a":1,"\u0061":2}"#).is_err());
        assert!(strict_json(br#"{"payload":{"x":1,"x":2}}"#).is_err());
        let encoded=URL_SAFE_NO_PAD.encode(r#"{"alg":"none","kid":"ed25519:0000000000000000000000000000000000000000000","typ":"cc-sakura-request-v1+jws"}"#);
        assert!(header(&encoded).is_err());
        assert!(decode("YQ==", 32).is_err());
        Ok(())
    }
    #[test]
    fn keygen_never_overwrites_and_private_permissions_are_checked() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("key.pem");
        let first = generate_key(&path)?;
        assert!(generate_key(&path).is_err());
        let key = SigningKey::from_pkcs8_pem(&fs::read_to_string(&path)?)?;
        assert_eq!(first["key_id"], key_id(key.verifying_key().as_bytes()));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o644))?;
            assert!(private_existing(&path).is_err());
        }
        Ok(())
    }
    #[test]
    fn project_targets_cannot_be_inferred_from_bare_names() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let client = client(dir.path())?;
        assert!(client.target(&json!({"room":"project"})).is_err());
        assert!(
            client
                .target(&json!({"room":"old-room","project":"owner/project"}))
                .is_err()
        );
        assert!(client.target(&json!({"project":"other/project"})).is_err());
        assert_eq!(
            client.target(&json!({"room":"old-room","after":4}))?,
            json!({"after":4})
        );
        assert_eq!(
            client.target(&json!({"project":"owner/project"}))?,
            json!({})
        );
        assert!(
            SignedClient::new(
                "http://example.org/owner/project",
                client.auth.clone(),
                CompatConfig::default()
            )
            .is_err()
        );
        Ok(())
    }
    #[test]
    fn outbox_survives_client_restart_and_is_scoped() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let client = client(dir.path())?;
        let id = Uuid::new_v4().to_string();
        let action = client.action("send", json!({"payload":[1,2]}), Some(id.clone()))?;
        client.outbox()?.execute(
            "INSERT INTO signed_outbox VALUES(?1,?2,?3,?4,?5,NULL,1)",
            params![
                id,
                client.auth.server_id,
                client.auth.project_id,
                client.auth.member_id,
                action.get()
            ],
        )?;
        let again = SignedClient::new(
            &client.project_url,
            client.auth.clone(),
            CompatConfig::default(),
        )?;
        let saved: String = again.outbox()?.query_row(
            "SELECT action FROM signed_outbox WHERE id=?1",
            [id],
            |r| r.get(0),
        )?;
        assert_eq!(saved, action.get());
        Ok(())
    }
    #[tokio::test]
    async fn ambiguous_response_retries_same_action_with_fresh_request_signature() -> Result<()> {
        use axum::{Router, extract::State, response::IntoResponse, routing::post};
        use std::sync::{
            Mutex,
            atomic::{AtomicUsize, Ordering},
        };
        struct Fixture {
            public: String,
            requests: Mutex<Vec<SignedRequest>>,
            count: AtomicUsize,
        }
        async fn receive(
            State(state): State<Arc<Fixture>>,
            body: String,
        ) -> axum::response::Response {
            let request = parse_and_verify(&body, &state.public).unwrap();
            if request.action().unwrap().op == "identity" {
                return axum::Json(json!({"encryption_key_id":null})).into_response();
            }
            state.requests.lock().unwrap().push(request);
            if state.count.fetch_add(1, Ordering::SeqCst) == 0 {
                // The update was committed, but the response body is unusable.
                (axum::http::StatusCode::OK, "interrupted response").into_response()
            } else {
                axum::Json(json!({"id":7})).into_response()
            }
        }
        let dir = tempfile::tempdir()?;
        let template = client(dir.path())?;
        let fixture = Arc::new(Fixture {
            public: URL_SAFE_NO_PAD.encode(template.key.verifying_key().as_bytes()),
            requests: Mutex::new(Vec::new()),
            count: AtomicUsize::new(0),
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let app = Router::new()
            .route("/owner/project/api/v1/rpc", post(receive))
            .with_state(fixture.clone());
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let mut auth = template.auth.clone();
        auth.allow_insecure_loopback = true;
        let client = SignedClient::new(
            &format!("http://{address}/owner/project"),
            auth,
            CompatConfig::default(),
        )?;
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
        assert!(
            client
                .call(&http, "send", json!({"payload":{"result":"done"}}))
                .await
                .is_err()
        );
        let operation_id: String =
            client
                .outbox()?
                .query_row("SELECT id FROM signed_outbox", [], |r| r.get(0))?;
        assert_eq!(client.retry(&http, &operation_id).await?, json!({"id":7}));
        assert_eq!(client.retry(&http, &operation_id).await?, json!({"id":7}));
        assert_eq!(fixture.count.load(Ordering::SeqCst), 2);
        let requests = fixture.requests.lock().unwrap();
        assert_eq!(requests[0].action.get(), requests[1].action.get());
        assert_ne!(requests[0].request_id, requests[1].request_id);
        assert_eq!(requests[0].action()?.operation_id, Some(operation_id));
        drop(requests);
        task.abort();
        Ok(())
    }
}

#[cfg(test)]
mod invitation_tests {
    use super::*;
    #[test]
    fn invitation_file_is_strict_versioned_and_private() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("invite.json");
        let sample_invitation = URL_SAFE_NO_PAD.encode([9u8; 32]);
        let valid = json!({"version":1,"room_url":"https://example.com/owner/room","token":sample_invitation});
        save_private_profile(&path, &valid.to_string())?;
        assert_eq!(read_invitation(&path)?.invitation_code, sample_invitation);
        for invalid in [
            json!({"version":2,"room_url":"https://example.com/owner/room","token":sample_invitation}).to_string(),
            json!({"version":1,"room_url":"https://example.com/owner/room","token":"bad"}).to_string(),
            json!({"version":1,"room_url":"https://example.com/owner/room","token":sample_invitation,"key":"unexpected"}).to_string(),
            format!("{{\"version\":1,\"version\":1,\"room_url\":\"https://example.com/owner/room\",\"token\":\"{sample_invitation}\"}}"),
            "x".repeat(8193),
        ] {
            fs::write(&path,invalid)?;
            assert!(read_invitation(&path).is_err());
        }
        assert!(read_invitation(Path::new("invite.json")).is_err());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::write(&path, valid.to_string())?;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o644))?;
            assert!(read_invitation(&path).is_err());
        }
        Ok(())
    }
}
