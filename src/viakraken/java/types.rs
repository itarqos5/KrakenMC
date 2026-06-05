use uuid::Uuid;

#[derive(Debug, Clone)]
pub(super) struct HandshakeInfo {
    pub protocol_version: i32,
    pub next_state: i32,
}

#[derive(Debug, Clone)]
pub(super) struct LoginStartInfo {
    pub username: String,
}

#[derive(Debug, Clone)]
pub(super) struct LoginSuccessCore {
    pub uuid: Uuid,
    pub username: String,
    pub properties: Vec<Property>,
}

#[derive(Debug, Clone)]
pub(super) struct Property {
    pub name: String,
    pub value: String,
    pub signature: Option<String>,
}
