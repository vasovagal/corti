use corti_postprocess::PromptTask;
use serde_json::{Value, json};

pub(crate) fn output_schema(task: PromptTask) -> Value {
    match task {
        PromptTask::Rewrite => json!({
            "type": "object",
            "properties": {
                "schema": {"type": "integer", "const": 1},
                "replacements": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "row_id": {"type": "string"},
                            "text": {"type": "string"}
                        },
                        "required": ["row_id", "text"],
                        "additionalProperties": false
                    }
                }
            },
            "required": ["schema", "replacements"],
            "additionalProperties": false
        }),
        PromptTask::Question => json!({
            "type": "object",
            "properties": {
                "schema": {"type": "integer", "const": 1},
                "answer": {"type": "string"},
                "cited_row_ids": {
                    "type": "array",
                    "items": {"type": "string"}
                },
                "context_truncated": {"type": "boolean"}
            },
            "required": ["schema", "answer", "cited_row_ids", "context_truncated"],
            "additionalProperties": false
        }),
    }
}

pub(crate) fn output_schema_name(task: PromptTask) -> &'static str {
    match task {
        PromptTask::Rewrite => "corti_rewrite_v1",
        PromptTask::Question => "corti_question_v1",
    }
}
