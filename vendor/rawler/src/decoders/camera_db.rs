// SPDX-License-Identifier: LGPL-2.1
// PES patch: precompiled camera database.
//
// `data/join.rs` serialises every camera (base table, modes, aliases) once into
// a compact binary instead of the 306 KB cameras.toml. A lookup decodes only
// the matching camera and replays the old loader's base-then-mode
// `Camera::update_from_toml` sequence, so first use costs a few hundred
// microseconds instead of 127 ms of TOML parsing on wasm.

use std::collections::HashMap;

use toml::Value;
use toml::value::Table;

use super::Camera;

const MAGIC: &[u8; 8] = b"PESCDB4\n";

static DB_BYTES: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/camera_db.bin"));

#[derive(Clone, Copy)]
pub struct CameraDb {
  bytes: &'static [u8],
}

impl std::fmt::Debug for CameraDb {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("CameraDb").field("bytes", &self.bytes.len()).finish()
  }
}

impl Default for CameraDb {
  fn default() -> Self {
    Self { bytes: &[] }
  }
}

impl CameraDb {
  pub fn new() -> Self {
    let bytes = DB_BYTES;
    if bytes.len() < MAGIC.len() || &bytes[..MAGIC.len()] != MAGIC {
      panic!("camera_db.bin missing or has an unknown version");
    }
    Self { bytes }
  }

  /// Exact (make, model, mode) match. Later entries win, matching the old
  /// HashMap insert order.
  pub fn lookup(&self, make: &str, model: &str, mode: &str) -> Option<Camera> {
    if self.bytes.is_empty() {
      return None;
    }
    let mut r = Reader::new(self.bytes, MAGIC.len());
    let count = r.u32();
    let mut found: Option<Hit<'_>> = None;
    for _ in 0..count {
      let e = parse_entry(&mut r);
      let mut clean = if e.make == make && e.base_model == model { Some(e.base_clean_model) } else { None };
      for (alias_model, alias_clean) in &e.aliases {
        if e.make == make && *alias_model == model {
          clean = Some(*alias_clean);
        }
      }
      if let Some(clean) = clean {
        let mut matched = e.base_mode == mode;
        let mut mode_at: Option<(usize, usize)> = None;
        for (name, start, len) in &e.mode_ranges {
          if *name == mode {
            matched = true;
            mode_at = Some((*start, *len));
          }
        }
        if matched {
          found = Some(Hit { base_start: e.base_start, mode_at, clean });
        }
      }
    }
    found.map(|hit| self.decode(&hit, model))
  }

  /// Cameras registered by filesize (CHDK-style naked files). Last match wins.
  pub fn by_filesize(&self, len: usize) -> Option<Camera> {
    if self.bytes.is_empty() {
      return None;
    }
    let mut r = Reader::new(self.bytes, MAGIC.len());
    let cameras = r.u32();
    for _ in 0..cameras {
      parse_entry(&mut r);
    }
    let count = r.u32();
    let mut found: Option<(u32, u32, u32)> = None;
    for _ in 0..count {
      let filesize = r.u64();
      let cam_idx = r.u32();
      let alias_idx = r.u32();
      let mode_idx = r.u32();
      if filesize == len as u64 {
        found = Some((cam_idx, alias_idx, mode_idx));
      }
    }
    let (cam_idx, alias_idx, mode_idx) = found?;

    let mut r = Reader::new(self.bytes, MAGIC.len());
    let cameras = r.u32();
    for idx in 0..cameras {
      let e = parse_entry(&mut r);
      if idx != cam_idx {
        continue;
      }
      let (model, clean) = if alias_idx == 0 {
        (e.base_model, e.base_clean_model)
      } else {
        *e.aliases.get(alias_idx as usize - 1)?
      };
      let mut hit = Hit { base_start: e.base_start, mode_at: None, clean };
      if mode_idx > 0 {
        let (_name, start, len) = *e.mode_ranges.get(mode_idx as usize - 1)?;
        hit.mode_at = Some((start, len));
      }
      return Some(self.decode(&hit, model));
    }
    None
  }

  /// Materialise every camera. Used by the public loader API only; not on the
  /// decode path.
  pub fn all(&self) -> HashMap<(String, String, String), Camera> {
    let mut map = HashMap::new();
    if self.bytes.is_empty() {
      return map;
    }
    let mut r = Reader::new(self.bytes, MAGIC.len());
    let count = r.u32();
    for _ in 0..count {
      let e = parse_entry(&mut r);
      let mut base_cam = Camera::new();
      base_cam.update_from_toml(&Reader::new(self.bytes, e.base_start).table());
      for (model, clean) in std::iter::once((e.base_model, e.base_clean_model)).chain(e.aliases.iter().copied()) {
        let mut mcam = base_cam.clone();
        mcam.model = model.to_string();
        mcam.clean_model = clean.to_string();
        map.insert((mcam.make.clone(), mcam.model.clone(), mcam.mode.clone()), mcam);
        for (_name, start, _len) in &e.mode_ranges {
          let mut mcam = base_cam.clone();
          mcam.update_from_toml(&Reader::new(self.bytes, *start).table());
          mcam.model = model.to_string();
          mcam.clean_model = clean.to_string();
          map.insert((mcam.make.clone(), mcam.model.clone(), mcam.mode.clone()), mcam);
        }
      }
    }
    map
  }

  fn decode(&self, hit: &Hit<'_>, model: &str) -> Camera {
    let mut cam = Camera::new();
    cam.update_from_toml(&Reader::new(self.bytes, hit.base_start).table());
    if let Some((start, _len)) = hit.mode_at {
      cam.update_from_toml(&Reader::new(self.bytes, start).table());
    }
    cam.model = model.to_string();
    cam.clean_model = hit.clean.to_string();
    cam
  }
}

struct Hit<'a> {
  base_start: usize,
  /// `None` for the base table, `Some` for an extra mode table.
  mode_at: Option<(usize, usize)>,
  clean: &'a str,
}

struct Entry<'a> {
  make: &'a str,
  base_model: &'a str,
  base_clean_model: &'a str,
  base_mode: &'a str,
  aliases: Vec<(&'a str, &'a str)>,
  /// (mode name, payload start, payload length)
  mode_ranges: Vec<(&'a str, usize, usize)>,
  base_start: usize,
}

fn parse_entry<'a>(r: &mut Reader<'a>) -> Entry<'a> {
  let make = r.str();
  let base_model = r.str();
  let base_clean_model = r.str();
  let base_mode = r.str();
  let alias_count = r.u16();
  let mut aliases = Vec::with_capacity(alias_count as usize);
  for _ in 0..alias_count {
    let model = r.str();
    let clean = r.str();
    aliases.push((model, clean));
  }
  let mode_count = r.u16();
  let mut mode_ranges = Vec::with_capacity(mode_count as usize);
  for _ in 0..mode_count {
    let name = r.str();
    let len = r.u32() as usize;
    let start = r.pos;
    r.pos += len;
    mode_ranges.push((name, start, len));
  }
  let len = r.u32() as usize;
  let base_start = r.pos;
  r.pos += len;
  Entry { make, base_model, base_clean_model, base_mode, aliases, mode_ranges, base_start }
}

struct Reader<'a> {
  bytes: &'a [u8],
  pos: usize,
}

impl<'a> Reader<'a> {
  fn new(bytes: &'a [u8], pos: usize) -> Self {
    Self { bytes, pos }
  }

  fn u8(&mut self) -> u8 {
    let v = self.bytes[self.pos];
    self.pos += 1;
    v
  }

  fn u16(&mut self) -> u16 {
    let v = u16::from_le_bytes(self.bytes[self.pos..self.pos + 2].try_into().expect("truncated camera db"));
    self.pos += 2;
    v
  }

  fn u32(&mut self) -> u32 {
    let v = u32::from_le_bytes(self.bytes[self.pos..self.pos + 4].try_into().expect("truncated camera db"));
    self.pos += 4;
    v
  }

  fn u64(&mut self) -> u64 {
    let v = u64::from_le_bytes(self.bytes[self.pos..self.pos + 8].try_into().expect("truncated camera db"));
    self.pos += 8;
    v
  }

  fn f64(&mut self) -> f64 {
    let v = f64::from_le_bytes(self.bytes[self.pos..self.pos + 8].try_into().expect("truncated camera db"));
    self.pos += 8;
    v
  }

  fn str(&mut self) -> &'a str {
    let len = self.u16() as usize;
    let s = std::str::from_utf8(&self.bytes[self.pos..self.pos + len]).expect("camera db string is invalid utf-8");
    self.pos += len;
    s
  }

  fn table(&mut self) -> Table {
    let count = self.u16();
    let mut table = Table::new();
    for _ in 0..count {
      let key = self.str().to_string();
      table.insert(key, self.value());
    }
    table
  }

  fn value(&mut self) -> Value {
    match self.u8() {
      0 => Value::Integer(self.i32() as i64),
      // f32: every non-params float is read from Camera as f32, so this
      // reproduces the exact same Camera field bits at half the bytes.
      1 => Value::Float(self.f32() as f64),
      2 => Value::String(self.str().to_string()),
      3 => Value::Boolean(self.u8() != 0),
      4 => {
        let count = self.u16();
        let mut items = Vec::with_capacity(count as usize);
        for _ in 0..count {
          items.push(self.value());
        }
        Value::Array(items)
      }
      5 => Value::Table(self.table()),
      6 => Value::Integer(self.i64()),
      7 => Value::Float(self.f64()),
      tag => panic!("Unknown value tag {} in camera db", tag),
    }
  }

  fn f32(&mut self) -> f32 {
    let v = f32::from_le_bytes(self.bytes[self.pos..self.pos + 4].try_into().expect("truncated camera db"));
    self.pos += 4;
    v
  }

  fn i32(&mut self) -> i32 {
    let v = i32::from_le_bytes(self.bytes[self.pos..self.pos + 4].try_into().expect("truncated camera db"));
    self.pos += 4;
    v
  }

  fn i64(&mut self) -> i64 {
    let v = i64::from_le_bytes(self.bytes[self.pos..self.pos + 8].try_into().expect("truncated camera db"));
    self.pos += 8;
    v
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  /// The loader as it existed before the precompiled DB: parse the joined
  /// cameras.toml, expand aliases x modes, insert into maps in order.
  fn old_loader() -> (HashMap<(String, String, String), Camera>, HashMap<usize, Camera>) {
    let toml = include_str!(concat!(env!("OUT_DIR"), "/cameras_ref.toml"));
    let toml = toml.parse::<Value>().expect("parse cameras_ref.toml");
    let mut cams = Vec::new();
    for camera in toml.get("cameras").expect("cameras").as_array().expect("cameras array") {
      let mut cammodes = Vec::new();
      let ct = camera.as_table().expect("camera table");
      cammodes.push(ct);
      if let Some(val) = ct.get("modes") {
        for mode in val.as_array().expect("modes array") {
          cammodes.push(mode.as_table().expect("mode table"));
        }
      }
      let mut cam = Camera::new();
      cam.update_from_toml(cammodes[0]);
      let mut camnames = vec![(cam.model.clone(), cam.clean_model.clone())];
      if let Some(val) = ct.get("model_aliases") {
        for alias in val.as_array().expect("aliases array") {
          camnames.push((
            alias[0].as_str().expect("alias model").to_string(),
            alias[1].as_str().expect("alias clean_model").to_string(),
          ));
        }
      }
      for (model, clean_model) in camnames {
        for ct in cammodes.clone() {
          let mut mcam = cam.clone();
          mcam.update_from_toml(ct);
          mcam.model = model.clone();
          mcam.clean_model = clean_model.clone();
          cams.push(mcam);
        }
      }
    }
    let mut map = HashMap::new();
    let mut naked = HashMap::new();
    for cam in cams {
      map.insert((cam.make.clone(), cam.model.clone(), cam.mode.clone()), cam.clone());
      if cam.filesize > 0 {
        naked.insert(cam.filesize, cam);
      }
    }
    (map, naked)
  }

  fn norm(cam: &Camera) -> String {
    let mut color: Vec<_> = cam.color_matrix.iter().map(|(k, v)| (format!("{:?}", k), format!("{:?}", v))).collect();
    color.sort();
    let mut params: Vec<_> = cam.params.iter().map(|(k, v)| (k.clone(), format!("{:?}", v))).collect();
    params.sort();
    let parts = [
      format!("{:?}", cam.make),
      format!("{:?}", cam.model),
      format!("{:?}", cam.mode),
      format!("{:?}", cam.clean_make),
      format!("{:?}", cam.clean_model),
      format!("{:?}", cam.remark),
      cam.filesize.to_string(),
      format!("{}x{}", cam.raw_width, cam.raw_height),
      format!("{:?}", cam.whitelevel),
      format!("{:?}", cam.blacklevel),
      format!("{:?}", cam.blackareah),
      format!("{:?}", cam.blackareav),
      format!("{:?}", cam.xyz_to_cam),
      format!("{:?}", color),
      format!("{:?}", cam.cfa),
      format!("{:?}", cam.plane_color),
      format!("{:?}", cam.active_area),
      format!("{:?}", cam.crop_area),
      format!("{:?}", cam.bps),
      cam.real_bps.to_string(),
      cam.highres_width.to_string(),
      format!("{:?}", cam.default_scale),
      format!("{:?}", cam.best_quality_scale),
      format!("{:?}", cam.hints),
      format!("{:?}", params),
    ];
    parts.join(" ")
  }

  #[test]
  fn camera_db_matches_original_loader() {
    let (want, want_naked) = old_loader();
    let db = CameraDb::new();
    let got = db.all();
    assert_eq!(got.len(), want.len(), "camera count");
    for (key, expected) in &want {
      let actual = got.get(key).unwrap_or_else(|| panic!("missing camera {:?}", key));
      assert_eq!(norm(actual), norm(expected), "camera mismatch for {:?}", key);
    }
    for (filesize, expected) in &want_naked {
      let actual = db
        .by_filesize(*filesize)
        .unwrap_or_else(|| panic!("missing naked camera {}", filesize));
      assert_eq!(norm(&actual), norm(expected), "naked camera mismatch for {}", filesize);
    }
  }

  #[test]
  fn camera_db_lookup_finds_every_camera() {
    let (want, _) = old_loader();
    let db = CameraDb::new();
    for ((make, model, mode), expected) in &want {
      let actual = db
        .lookup(make, model, mode)
        .unwrap_or_else(|| panic!("lookup missed {:?}", (make, model, mode)));
      assert_eq!(norm(&actual), norm(expected), "lookup mismatch for {:?}", (make, model, mode));
    }
  }
}
