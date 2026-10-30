use std::env;
use std::fs::File;
use std::io::prelude::*;
use std::path::Path;

extern crate glob;
use self::glob::glob;
extern crate toml;
use toml::Value;
extern crate rustc_version;
use rustc_version::{Version, version};

fn main() {
  join_cameras();
  join_lenses();
}

struct CameraEntry {
  make: String,
  base_model: String,
  base_clean_model: String,
  base_mode: String,
  aliases: Vec<(String, String)>,
  modes: Vec<(String, toml::value::Table)>,
  base: toml::value::Table,
}

/// PES patch: instead of shipping cameras.toml and parsing it on the first
/// RAW open (127 ms on wasm), emit a compact binary holding each camera table
/// once. The runtime parses only the camera it looks up and replays the old
/// loader's base-then-mode `Camera::update_from_toml` sequence, so aliases and
/// modes cost no duplicated payload.
fn join_cameras() {
  let out_dir = env::var("OUT_DIR").expect("Missing ENV OUT_DIR");

  let mut joined = String::new();
  for entry in glob("./data/cameras/*/**/*.toml").expect("Failed to read glob pattern") {
    joined.push_str("[[cameras]]\n");
    let path = entry.expect("Invalid glob entry");
    let mut f = File::open(&path).expect("failed to open camera definition file");
    let mut toml = String::new();
    f.read_to_string(&mut toml).expect("Failed to read camera definition file");
    match toml.parse::<Value>() {
      Ok(_) => {}
      Err(e) => panic!("Error parsing {:?}: {:?}", path, e),
    };
    joined.push_str(&toml);
    joined.push('\n');
  }

  let root = joined.parse::<Value>().expect("Error parsing joined cameras.toml");
  let cameras = root
    .get("cameras")
    .expect("cameras.toml missing 'cameras' key")
    .as_array()
    .expect("'cameras' must be an array");

  let mut entries: Vec<CameraEntry> = Vec::new();
  for camera in cameras {
    let base = camera.as_table().expect("each camera entry must be a table");
    let mut base_payload = base.clone();
    let aliases = match base.get("model_aliases") {
      Some(val) => val
        .as_array()
        .expect("'model_aliases' must be an array")
        .iter()
        .map(|alias| {
          (
            alias[0].as_str().expect("model alias name must be a string").to_string(),
            alias[1].as_str().expect("model alias clean_name must be a string").to_string(),
          )
        })
        .collect(),
      None => Vec::new(),
    };
    let modes = match base.get("modes") {
      Some(val) => val
        .as_array()
        .expect("'modes' must be an array")
        .iter()
        .map(|mode| {
          let table = mode.as_table().expect("each mode entry must be a table").clone();
          let name = table.get("mode").and_then(|v| v.as_str()).unwrap_or("").to_string();
          (name, table)
        })
        .collect(),
      None => Vec::new(),
    };
    // The binary keeps aliases and modes in its index; neither is read by
    // Camera::update_from_toml.
    base_payload.remove("modes");
    base_payload.remove("model_aliases");

    entries.push(CameraEntry {
      make: base.get("make").and_then(|v| v.as_str()).unwrap_or("").to_string(),
      base_model: base
        .get("model")
        .and_then(|v| v.as_str())
        .expect("camera model must be a string")
        .to_string(),
      base_clean_model: base
        .get("clean_model")
        .and_then(|v| v.as_str())
        .expect("camera clean_model must be a string")
        .to_string(),
      base_mode: base.get("mode").and_then(|v| v.as_str()).unwrap_or("").to_string(),
      aliases,
      modes,
      base: base_payload,
    });
  }

  // CHDK-style naked files register by file size; the old loader's HashMap
  // kept the last insert (cameras in glob order, aliases outer, modes inner).
  let mut naked: Vec<(u64, u32, u32, u32)> = Vec::new();
  let mut naked_pos: std::collections::HashMap<u64, usize> = std::collections::HashMap::new();
  for (cam_idx, e) in entries.iter().enumerate() {
    let base_filesize = e.base.get("filesize").and_then(|v| v.as_integer()).unwrap_or(0);
    for alias_idx in 0..=e.aliases.len() {
      for mode_idx in 0..=e.modes.len() {
        // A mode table may override filesize; later tables win like scalars.
        let filesize = if mode_idx == 0 {
          base_filesize
        } else {
          e.modes[mode_idx - 1]
            .1
            .get("filesize")
            .and_then(|v| v.as_integer())
            .unwrap_or(base_filesize)
        };
        if filesize > 0 {
          let rec = (filesize as u64, cam_idx as u32, alias_idx as u32, mode_idx as u32);
          match naked_pos.get(&rec.0) {
            Some(&i) => naked[i] = rec,
            None => {
              naked_pos.insert(rec.0, naked.len());
              naked.push(rec);
            }
          }
        }
      }
    }
  }

  let mut out: Vec<u8> = b"PESCDB4\n".to_vec();
  out.extend((entries.len() as u32).to_le_bytes());
  for e in &entries {
    write_str(&mut out, &e.make);
    write_str(&mut out, &e.base_model);
    write_str(&mut out, &e.base_clean_model);
    write_str(&mut out, &e.base_mode);
    out.extend((e.aliases.len() as u16).to_le_bytes());
    for (model, clean) in &e.aliases {
      write_str(&mut out, model);
      write_str(&mut out, clean);
    }
    out.extend((e.modes.len() as u16).to_le_bytes());
    for (name, table) in &e.modes {
      write_str(&mut out, name);
      let mut payload = Vec::new();
      write_table(&mut payload, table, false);
      out.extend((payload.len() as u32).to_le_bytes());
      out.extend(payload);
    }
    let mut payload = Vec::new();
    write_table(&mut payload, &e.base, false);
    out.extend((payload.len() as u32).to_le_bytes());
    out.extend(payload);
  }
  out.extend((naked.len() as u32).to_le_bytes());
  for (filesize, cam_idx, alias_idx, mode_idx) in &naked {
    out.extend(filesize.to_le_bytes());
    out.extend(cam_idx.to_le_bytes());
    out.extend(alias_idx.to_le_bytes());
    out.extend(mode_idx.to_le_bytes());
  }

  let dest_path = Path::new(&out_dir).join("camera_db.bin");
  File::create(dest_path)
    .expect("Unable to create camera_db.bin")
    .write_all(&out)
    .expect("Failed to write camera_db.bin");

  // Test-only reference: camera_db's parity test re-parses this with the old
  // loader algorithm. It is never included in the wasm binary.
  let ref_path = Path::new(&out_dir).join("cameras_ref.toml");
  File::create(ref_path)
    .expect("Unable to create cameras_ref.toml")
    .write_all(joined.as_bytes())
    .expect("Failed to write cameras_ref.toml");

  // Check for a minimum version
  if version().expect("version failed") < Version::parse("1.31.0").expect("version parse failed") {
    println!("cargo:rustc-cfg=needs_chunks_exact");
  }
}

fn write_str(out: &mut Vec<u8>, s: &str) {
  assert!(s.len() <= u16::MAX as usize, "camera db string too long");
  out.extend((s.len() as u16).to_le_bytes());
  out.extend(s.as_bytes());
}

/// `exact_floats` is set for `params` subtrees: `Camera::params` stores the
/// raw `toml::Value`, so those floats must round-trip exactly. Every other
/// float in a camera table is consumed as f32 by `Camera::update_from_toml`
/// (the color matrix), so storing f32 there is bit-identical and half the size.
fn write_table(out: &mut Vec<u8>, table: &toml::value::Table, exact_floats: bool) {
  assert!(table.len() <= u16::MAX as usize, "camera db table too large");
  out.extend((table.len() as u16).to_le_bytes());
  for (key, value) in table {
    write_str(out, key);
    write_value(out, value, exact_floats || key == "params");
  }
}

fn write_value(out: &mut Vec<u8>, value: &Value, exact_floats: bool) {
  match value {
    Value::Integer(i) if *i >= i32::MIN as i64 && *i <= i32::MAX as i64 => {
      out.push(0);
      out.extend((*i as i32).to_le_bytes());
    }
    Value::Integer(i) => {
      out.push(6);
      out.extend(i.to_le_bytes());
    }
    Value::Float(f) if exact_floats => {
      out.push(7);
      out.extend(f.to_le_bytes());
    }
    Value::Float(f) => {
      out.push(1);
      out.extend((*f as f32).to_le_bytes());
    }
    Value::String(s) => {
      out.push(2);
      write_str(out, s);
    }
    Value::Boolean(b) => {
      out.push(3);
      out.push(*b as u8);
    }
    Value::Array(items) => {
      assert!(items.len() <= u16::MAX as usize, "camera db array too large");
      out.push(4);
      out.extend((items.len() as u16).to_le_bytes());
      for item in items {
        write_value(out, item, exact_floats);
      }
    }
    Value::Table(table) => {
      out.push(5);
      write_table(out, table, exact_floats);
    }
    other => panic!("Unsupported TOML value in camera db: {:?}", other),
  }
}

fn join_lenses() {
  let out_dir = env::var("OUT_DIR").expect("Missing ENV OUT_DIR");
  let dest_path = Path::new(&out_dir).join("lenses.toml");
  let mut out = File::create(dest_path).expect("Unable to create output file");

  for entry in glob("./data/lenses/*/**/*.toml").expect("Failed to read glob pattern") {
    let path = entry.expect("Invalid glob entry");
    let mut f = File::open(&path).expect("failed to open lens definition file");
    let mut toml = String::new();
    f.read_to_string(&mut toml).expect("Failed to read lens definition file");

    {
      match toml.parse::<Value>() {
        Ok(_) => {}
        Err(e) => panic!("Error parsing {:?}: {:?}", path, e),
      };
    }

    out.write_all(&toml.into_bytes()).expect("Failed to write");
    out.write_all(b"\n").expect("Failed to write");
  }

  // Check for a minimum version
  if version().expect("version failed") < Version::parse("1.31.0").expect("version parse failed") {
    println!("cargo:rustc-cfg=needs_chunks_exact");
  }
}
