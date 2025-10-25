use anyhow::{anyhow, Error};
use gst::glib::{self, prelude::*};
use gst::prelude::*;
use serde_json::{json, Map, Value};

fn gvalue_to_json_value(gvalue: &glib::Value) -> Result<Value, Error> {
    if let Ok(value) = gvalue.get::<i32>() {
        return Ok(json!(value));
    }
    if let Ok(value) = gvalue.get::<u32>() {
        return Ok(json!(value));
    }
    if let Ok(value) = gvalue.get::<i64>() {
        return Ok(json!(value));
    }
    if let Ok(value) = gvalue.get::<u64>() {
        return Ok(json!(value));
    }
    if let Ok(value) = gvalue.get::<f32>() {
        return Ok(json!(value));
    }
    if let Ok(value) = gvalue.get::<f64>() {
        return Ok(json!(value));
    }
    if let Ok(value) = gvalue.get::<bool>() {
        return Ok(json!(value));
    }

    if let Ok(value) = gvalue.get::<String>() {
        return Ok(json!(value));
    }
    if let Ok(value) = gvalue.get::<gst::Fraction>() {
        return Ok(json!([value.numer(), value.denom()]));
    }
    if let Ok(list) = gvalue.get::<gst::List>() {
        let values: Result<Vec<Value>, _> = list
            .iter()
            .map(|v| gvalue_to_json_value(&v.to_value()))
            .collect();
        return Ok(json!(values?));
    }

    Err(anyhow!(
        "Unsupported GType for JSON conversion: {}",
        gvalue.type_()
    ))
}

fn structure_to_json_value(structure: &gst::StructureRef) -> Result<Value, Error> {
    let mut map = Map::new();
    map.insert(
        "media_type".to_string(),
        json!(structure.name().to_string()),
    );

    for (field_name, gvalue) in structure.iter() {
        let json_value = gvalue_to_json_value(&gvalue.to_value())?;
        map.insert(field_name.to_string(), json_value);
    }

    Ok(Value::Object(map))
}

pub fn caps_to_json_string(caps: &gst::Caps) -> Result<String, Error> {
    if caps.is_empty() || caps.is_any() {
        return Ok("{}".to_string());
    }

    let structure = caps
        .structure(0)
        .ok_or_else(|| anyhow!("Caps has no structure"))?;
    let json_value = structure_to_json_value(structure)?;

    Ok(serde_json::to_string(&json_value)?)
}
