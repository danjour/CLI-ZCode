//! Respostas automáticas aos requests do servidor.
//!
//! Regra do plano §3.1: se não responder, a criação de sessão falha com
//! ZodError -32603. O campo `nativeSearchEnhancementsEnabled: bool` é obrigatório.

use serde_json::Value;

/// Gera o `result` para um request do servidor.
pub fn answer(method: &str, _params: &Value) -> Value {
    match method {
        "session/requestRuntimePreferences" => {
            serde_json::json!({ "nativeSearchEnhancementsEnabled": false })
        }
        // Resposta genérica p/ não travar o runtime em métodos futuros.
        // Se o protocolo ganhar método novo obrigatório, o orquestrador deve
        // ser chamado (condição de parada do handoff).
        _ => serde_json::json!({}),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_preferences_tem_campo_obrigatorio() {
        let r = answer("session/requestRuntimePreferences", &Value::Null);
        assert_eq!(r["nativeSearchEnhancementsEnabled"], false);
    }

    #[test]
    fn metodo_desconhecido_nao_trava() {
        let r = answer("futuro/metodo", &Value::Null);
        assert!(r.is_object());
    }
}
