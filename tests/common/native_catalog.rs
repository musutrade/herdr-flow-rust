use serde_json::{json, Value};

// Synthetic recursive MCP input schema with the depth-26 envelope observed on
// Codex 0.165.0. No real tool names, definitions or configuration are retained.
pub fn catalog() -> Value {
    let schema = (0..9).fold(
        json!({"type":"string"}),
        |inner, _| json!({"properties":{"field":inner}}),
    );
    json!({"id":91,"result":{"data":[{"tools":{"example":{
        "inputSchema":{"items":schema}
    }}}],"nextCursor":null}})
}

pub fn nested_message(depth: usize) -> Value {
    (0..depth).fold(Value::Null, |inner, _| json!({"nested":inner}))
}

pub fn tool_start(depth: usize, namespace: bool) -> Value {
    let offset = if namespace { 6 } else { 4 };
    let function = json!({"type":"function","name":"example","description":"fixture",
        "inputSchema":nested_message(depth - offset)});
    let tool = if namespace {
        json!({"type":"namespace","name":"fixture","description":"fixture","tools":[function]})
    } else {
        function
    };
    json!({"id":7,"method":"thread/start","params":{"dynamicTools":[tool]}})
}
