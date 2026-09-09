//! Transporte JSON-RPC newline-delimited (ZCode Protocol v1).
//!
//! - SEM campo `jsonrpc`.
//! - Requests do cliente: `id` numérico.
//! - Requests do servidor: `id` string (ex. `"server-1"`).
//! - Uma linha = uma mensagem JSON.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum RpcError {
    #[error("RPC erro {code}: {message}")]
    Server { code: i64, message: String },
    #[error("resposta sem result nem error (id={0})")]
    EmptyResponse(String),
    /// Variante semântica do transporte JSON-L (par da `to_line`); o runtime
    /// hoje ignora linha não-JSON, mas o daemon/broker registrado em
    /// docs/RESUMO.md ("Futuro registrado") deve reportá-la.
    #[allow(dead_code)]
    #[error("linha não é JSON: {0}")]
    BadLine(String),
    #[error("transporte: {0}")]
    Transport(String),
}

/// Id de mensagem: cliente usa número, servidor usa string.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum RequestId {
    Num(u64),
    Str(String),
}

impl std::fmt::Display for RequestId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RequestId::Num(n) => write!(f, "{n}"),
            RequestId::Str(s) => write!(f, "{s}"),
        }
    }
}

/// Monta um request do cliente (sem `jsonrpc`).
pub fn build_request(id: u64, method: &str, params: Value) -> Value {
    serde_json::json!({
        "id": id,
        "method": method,
        "params": params,
    })
}

/// Resposta a um request do servidor.
pub fn build_server_response(id: &RequestId, result: Value) -> Value {
    match id {
        RequestId::Num(n) => serde_json::json!({ "id": n, "result": result }),
        RequestId::Str(s) => serde_json::json!({ "id": s, "result": result }),
    }
}

/// Serializa uma mensagem em uma linha (sem `\n` interno).
pub fn to_line(v: &Value) -> String {
    serde_json::to_string(v).unwrap_or_else(|_| "{}".to_string())
}

/// Faz parse de uma linha. Retorna erro se não for JSON.
/// Par de `to_line` no contrato do transporte; o runtime hoje parseia inline,
/// mas o daemon/broker (docs/RESUMO.md "Futuro registrado") reusará isto.
#[allow(dead_code)]
pub fn parse_line(line: &str) -> Result<Value, RpcError> {
    serde_json::from_str(line).map_err(|_| RpcError::BadLine(line.to_string()))
}

/// É um request do servidor? (tem `method` + `id` string)
pub fn is_server_request(v: &Value) -> bool {
    v.get("method").and_then(|m| m.as_str()).is_some()
        && matches!(v.get("id"), Some(Value::String(_)))
}

/// É resposta a request nosso? (tem `id` numérico + result/error)
pub fn is_client_response(v: &Value) -> bool {
    matches!(v.get("id"), Some(Value::Number(_)))
        && (v.get("result").is_some() || v.get("error").is_some())
}

/// Quebra o id numérico de uma resposta.
pub fn response_id(v: &Value) -> Option<u64> {
    v.get("id")?.as_u64()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_sem_jsonrpc_e_id_numerico() {
        let r = build_request(1, "session/create", serde_json::json!({}));
        assert_eq!(r["id"], 1);
        assert_eq!(r["method"], "session/create");
        assert!(r.get("jsonrpc").is_none(), "não deve ter campo jsonrpc");
    }

    #[test]
    fn server_request_id_string() {
        let v: Value = serde_json::from_str(
            r#"{"id":"server-1","method":"session/requestRuntimePreferences","params":{"sessionId":"sess_x"}}"#,
        )
        .unwrap();
        assert!(is_server_request(&v));
        assert!(!is_client_response(&v));
        let resp = build_server_response(&RequestId::Str("server-1".into()), serde_json::json!({"nativeSearchEnhancementsEnabled": false}));
        assert_eq!(resp["id"], "server-1");
        assert_eq!(resp["result"]["nativeSearchEnhancementsEnabled"], false);
    }

    #[test]
    fn linha_invalida_da_erro() {
        assert!(parse_line("não json").is_err());
    }
}
