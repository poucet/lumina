use llm_macros::tool;
use std::collections::HashMap;

// Test 1: Basic function with simple parameters
#[tool]
fn add_numbers(a: i32, b: i32) -> i32 {
    a + b
}

#[test]
fn test_add_numbers_struct_generated() {
    // Test that the Args struct exists and has the correct fields
    let args = AddNumbersArgs { a: 5, b: 3 };
    assert_eq!(args.a, 5);
    assert_eq!(args.b, 3);
}

#[test]
fn test_add_numbers_tool_def() {
    // Test that tool_def is generated correctly
    let tool_def = AddNumbersArgs::tool_def();
    assert_eq!(tool_def.name, "add_numbers");
}

#[test]
fn test_add_numbers_call_wrapper() {
    // Test that the call wrapper works (sync function)
    let args_json = serde_json::json!({
        "a": 10,
        "b": 20
    });

    let result = AddNumbersArgs::call(args_json);
    assert!(result.is_ok());
    let result_str = result.unwrap();
    assert_eq!(result_str, "30");
}

// Test 2: Function with documentation
/// Multiplies two numbers together.
/// Returns the product of x and y.
#[tool]
fn multiply(x: i32, y: i32) -> i32 {
    x * y
}

#[test]
fn test_multiply_tool_def_has_description() {
    let tool_def = MultiplyArgs::tool_def();
    assert_eq!(tool_def.name, "multiply");
    assert!(tool_def.description.is_some());
    let desc = tool_def.description.unwrap();
    assert!(desc.contains("Multiplies two numbers"));
}

// Test 3: Async function
#[tool]
async fn fetch_data(url: String) -> String {
    format!("Fetched from: {}", url)
}

#[test]
fn test_fetch_data_async() {
    let args_json = serde_json::json!({
        "url": "https://example.com"
    });

    // Async functions require await
    let result = tokio_test::block_on(async {
        FetchDataArgs::call(args_json).await
    });

    assert!(result.is_ok());
    let result_str = result.unwrap();
    assert!(result_str.contains("example.com"));
}

// Test 4: Function with multiple parameter types
#[tool]
fn complex_function(
    number: i32,
    text: String,
    flag: bool,
    decimal: f64,
) -> String {
    format!("{} {} {} {}", number, text, flag, decimal)
}

#[test]
fn test_complex_function_multiple_types() {
    let args = ComplexFunctionArgs {
        number: 42,
        text: "test".to_string(),
        flag: true,
        decimal: 2.5,
    };

    assert_eq!(args.number, 42);
    assert_eq!(args.text, "test");
    assert!(args.flag);
    assert_eq!(args.decimal, 2.5);
}

// Test 5: Function with Vec parameter
#[tool]
fn process_list(items: Vec<String>) -> usize {
    items.len()
}

#[test]
fn test_process_list_vec_type() {
    let args_json = serde_json::json!({
        "items": ["a", "b", "c"]
    });

    let result = ProcessListArgs::call(args_json);
    assert!(result.is_ok());
    let result_str = result.unwrap();
    assert_eq!(result_str, "3");
}

// Test 6: Function with Option parameter
#[tool]
fn optional_param(required: String, optional: Option<i32>) -> String {
    match optional {
        Some(val) => format!("{}: {}", required, val),
        None => required,
    }
}

#[test]
fn test_optional_param_with_some() {
    let args_json = serde_json::json!({
        "required": "value",
        "optional": 42
    });

    let result = OptionalParamArgs::call(args_json);
    assert!(result.is_ok());
    let result_str = result.unwrap();
    assert!(result_str.contains("value: 42"));
}

#[test]
fn test_optional_param_with_none() {
    let args_json = serde_json::json!({
        "required": "value",
        "optional": null
    });

    let result = OptionalParamArgs::call(args_json);
    assert!(result.is_ok());
}

// Test 7: Function with snake_case to PascalCase conversion
#[tool]
fn my_long_function_name(value: i32) -> i32 {
    value * 2
}

#[test]
fn test_snake_case_to_pascal_case() {
    let args = MyLongFunctionNameArgs { value: 10 };
    assert_eq!(args.value, 10);
}

// Test 8: Function with Result return type
#[tool]
fn may_fail(should_fail: bool) -> Result<String, String> {
    if should_fail {
        Err("Failed".to_string())
    } else {
        Ok("Success".to_string())
    }
}

#[test]
fn test_may_fail_success() {
    let args_json = serde_json::json!({
        "should_fail": false
    });

    let result = MayFailArgs::call(args_json);
    assert!(result.is_ok());
}

// Test 9: Function with tuple parameter
#[tool]
fn process_tuple(coords: (i32, i32)) -> String {
    format!("({}, {})", coords.0, coords.1)
}

#[test]
fn test_process_tuple() {
    let args_json = serde_json::json!({
        "coords": [10, 20]
    });

    let result = ProcessTupleArgs::call(args_json);
    assert!(result.is_ok());
    let result_str = result.unwrap();
    assert!(result_str.contains("10"));
    assert!(result_str.contains("20"));
}

// Test 10: Function with no parameters
#[tool]
fn no_params() -> String {
    "constant".to_string()
}

#[test]
fn test_no_params() {
    let args_json = serde_json::json!({});

    let result = NoParamsArgs::call(args_json);
    assert!(result.is_ok());
    let result_str = result.unwrap();
    assert_eq!(result_str, "\"constant\"");
}

// Test 11: Public function visibility
#[tool]
pub fn public_function(x: i32) -> i32 {
    x
}

#[test]
fn test_public_function_visibility() {
    let args = PublicFunctionArgs { x: 5 };
    assert_eq!(args.x, 5);
}

// Test 12: Function with unit return type
#[tool]
fn returns_unit(value: i32) {
    let _ = value;
}

#[test]
fn test_returns_unit() {
    let args_json = serde_json::json!({
        "value": 42
    });

    let result = ReturnsUnitArgs::call(args_json);
    assert!(result.is_ok());
}

// Test 13: Async function with complex return type
#[tool]
async fn async_complex(id: String) -> Result<Vec<String>, String> {
    Ok(vec![id.clone(), format!("{}_copy", id)])
}

#[test]
fn test_async_complex() {
    let args_json = serde_json::json!({
        "id": "test123"
    });

    let result = tokio_test::block_on(async {
        AsyncComplexArgs::call(args_json).await
    });

    assert!(result.is_ok());
}

// Test 14: Function with parameter attributes preserved
#[tool]
fn with_attributes(
    #[serde(rename = "customName")]
    param: String,
) -> String {
    param
}

#[test]
fn test_with_attributes_serde_rename() {
    // Test that serde rename works
    let args_json = serde_json::json!({
        "customName": "test"
    });

    let result = WithAttributesArgs::call(args_json);
    assert!(result.is_ok());
}

// Test 15: Function with nested generic types
#[tool]
fn nested_generics(data: Vec<Option<String>>) -> usize {
    data.iter().filter(|x| x.is_some()).count()
}

#[test]
fn test_nested_generics() {
    let args_json = serde_json::json!({
        "data": ["hello", null, "world"]
    });

    let result = NestedGenericsArgs::call(args_json);
    assert!(result.is_ok());
    let result_str = result.unwrap();
    assert_eq!(result_str, "2");
}

// Test 16: Empty doc comment handling
#[allow(clippy::empty_docs)] // the empty doc comment is what this test exercises
///
#[tool]
fn empty_doc(x: i32) -> i32 {
    x
}

#[test]
fn test_empty_doc_comment() {
    let tool_def = EmptyDocArgs::tool_def();
    assert_eq!(tool_def.name, "empty_doc");
}

// Test 17: Multiple doc comments
/// First line.
///
/// Second paragraph.
/// More details here.
#[tool]
fn multi_doc(x: i32) -> i32 {
    x
}

#[test]
fn test_multiple_doc_comments() {
    let tool_def = MultiDocArgs::tool_def();
    assert!(tool_def.description.is_some());
}

// Test 18: Function with HashMap
#[tool]
fn process_map(data: HashMap<String, i32>) -> i32 {
    data.values().sum()
}

#[test]
fn test_process_map() {
    let args_json = serde_json::json!({
        "data": {
            "a": 1,
            "b": 2,
            "c": 3
        }
    });

    let result = ProcessMapArgs::call(args_json);
    assert!(result.is_ok());
    let result_str = result.unwrap();
    assert_eq!(result_str, "6");
}

// Test 19: Serialization/Deserialization of Args struct
#[test]
fn test_args_serialization() {
    let args = AddNumbersArgs { a: 10, b: 20 };

    // Serialize to JSON
    let json = serde_json::to_string(&args).unwrap();
    assert!(json.contains("10"));
    assert!(json.contains("20"));

    // Deserialize back
    let deserialized: AddNumbersArgs = serde_json::from_str(&json).unwrap();
    assert_eq!(deserialized.a, 10);
    assert_eq!(deserialized.b, 20);
}

// Test 20: JSON Schema generation
#[test]
fn test_json_schema_generation() {
    use schemars::schema_for;

    let schema = schema_for!(ComplexFunctionArgs);

    // Verify schema has the expected structure
    let schema_obj = schema.as_object().expect("schema should be an object");
    assert_eq!(
        schema_obj.get("type").and_then(serde_json::Value::as_str),
        Some("object")
    );
    assert!(schema_obj
        .get("properties")
        .and_then(serde_json::Value::as_object)
        .is_some());
}

// Test 21: Error handling in call wrapper
#[test]
fn test_call_wrapper_invalid_json() {
    let invalid_json = serde_json::json!({
        "wrong_field": 42
    });

    let result = AddNumbersArgs::call(invalid_json);
    // Should fail because required fields are missing
    assert!(result.is_err());
}

// Test 22: Tool definition name matches function name
#[test]
fn test_tool_def_name_matches() {
    assert_eq!(AddNumbersArgs::tool_def().name, "add_numbers");
    assert_eq!(MultiplyArgs::tool_def().name, "multiply");
    assert_eq!(FetchDataArgs::tool_def().name, "fetch_data");
    assert_eq!(MyLongFunctionNameArgs::tool_def().name, "my_long_function_name");
}

// Test 23: Function with single character parameter
#[tool]
fn single_char(a: i32) -> i32 {
    a
}

#[test]
fn test_single_character_param() {
    let args = SingleCharArgs { a: 5 };
    assert_eq!(args.a, 5);
}

// Test 24: pub(crate) visibility
#[tool]
pub(crate) fn internal_function(value: i32) -> i32 {
    value
}

#[test]
fn test_visibility_modifiers() {
    let args = InternalFunctionArgs { value: 42 };
    assert_eq!(args.value, 42);
}

// NOTE: Tests for methods with &self have been moved to tool_macro_method_tests.rs
// because the macro currently has a limitation where it generates Args structs
// inside impl blocks, which is invalid Rust syntax. Those tests are marked as
// #[ignore] until the macro is enhanced to properly support methods.
