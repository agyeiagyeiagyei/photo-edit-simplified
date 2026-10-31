//! WebGL2 parametric preview (WS4-C; design in docs/GPU.md).
//!
//! The worker hands us the pre-develop, pre-colour base at display-resolution
//! target dims; one fragment-shader draw reproduces the CPU pipeline in the
//! same order as `rawops::apply` + `color::apply_color_effects`, quantising at
//! the same u8 boundaries. The CPU path stays the export renderer and the
//! parity oracle. Auto mode is hardware-only: software rasterisers are an
//! order of magnitude slower than the CPU path here.

use std::cell::RefCell;
use std::collections::HashMap;

use js_sys::{Object, Reflect};
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use web_sys::{
    HtmlCanvasElement, WebGl2RenderingContext as Gl, WebGlFramebuffer, WebGlProgram,
    WebGlShader, WebGlTexture, WebGlUniformLocation, WebGlVertexArrayObject,
};

use pes_pixel::types::EditParams;
use pes_pixel::{color, rawops};

use crate::perf;

/// Identity of the base texture the GPU path needs. `need_*` is the target
/// render size, not the display size: a texture at least as large as the
/// display serves smaller windows too.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct BaseKey {
    pub item: u32,
    pub src_id: u32,
    pub src_fp: u64,
    pub rot90: u8,
    pub fine_bits: u32,
    pub need_w: u32,
    pub need_h: u32,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Auto,
    Force,
    Off,
}

struct State {
    mode: Option<Mode>,
    dead: bool,
    gpu: Option<Gpu>,
    base: Option<(BaseKey, u32, u32)>,
}

thread_local! {
    static STATE: RefCell<State> = RefCell::new(State {
        mode: None,
        dead: false,
        gpu: None,
        base: None,
    });
}

fn parse_mode() -> Mode {
    let search = web_sys::window()
        .and_then(|w| w.location().search().ok())
        .unwrap_or_default();
    for part in search.trim_start_matches('?').split('&') {
        match part {
            "gpu=1" => return Mode::Force,
            "gpu=0" => return Mode::Off,
            _ => {}
        }
    }
    Mode::Auto
}

/// Is the GPU path usable this session? Initialises lazily; a failure latches
/// it off and the CPU path runs untouched.
pub fn active() -> bool {
    STATE.with(|s| {
        let mut st = s.borrow_mut();
        if st.dead {
            return false;
        }
        let mode = match st.mode {
            Some(m) => m,
            None => {
                let m = parse_mode();
                st.mode = Some(m);
                m
            }
        };
        if mode == Mode::Off {
            return false;
        }
        if st.gpu.is_none() {
            match Gpu::new(mode == Mode::Force) {
                Ok(g) => st.gpu = Some(g),
                Err(e) => {
                    st.dead = true;
                    warn(&format!("gpu preview off: {e}"));
                    return false;
                }
            }
        }
        true
    })
}

pub fn latch_off() {
    STATE.with(|s| {
        let mut st = s.borrow_mut();
        st.dead = true;
        st.gpu = None;
        st.base = None;
    });
}

/// The shader implements no mask or spatial stages; those edits keep the CPU
/// path for the display render as well.
pub fn eligible(edit: &EditParams) -> bool {
    edit.selection.is_none() && edit.raw.denoise <= 0.0 && edit.raw.sharpen <= 0.0
}

/// `Some(stale_source)` when a base for the same item and geometry is large
/// enough; `None` when there is nothing usable.
pub fn base_match(key: &BaseKey) -> Option<bool> {
    STATE.with(|s| {
        let st = s.borrow();
        let gpu = st.gpu.as_ref()?;
        let (bk, bw, bh) = st.base.as_ref()?;
        if bk.item != key.item || bk.rot90 != key.rot90 || bk.fine_bits != key.fine_bits {
            return None;
        }
        // A pixel of slack keeps display/source rounding from re-requesting
        // the same base forever.
        if *bw + 2 < key.need_w || *bh + 2 < key.need_h {
            return None;
        }
        if gpu.base_dims != Some((*bw, *bh)) {
            return None;
        }
        Some(bk.src_id != key.src_id || bk.src_fp != key.src_fp)
    })
}

/// Upload a new base. Returns false if there is no GPU or the upload failed.
pub fn upload_base(key: BaseKey, pixels: &[u8], w: u32, h: u32) -> bool {
    if w == 0 || h == 0 || pixels.len() < (w as usize) * (h as usize) * 4 {
        return false;
    }
    STATE.with(|s| {
        let mut st = s.borrow_mut();
        let Some(gpu) = st.gpu.as_mut() else { return false };
        if gpu.upload_base(pixels, w, h).is_err() {
            st.dead = true;
            st.gpu = None;
            st.base = None;
            return false;
        }
        st.base = Some((key, w, h));
        true
    })
}

/// Draw the current edit from the uploaded base. Returns the offscreen canvas
/// and its display dims, or None if the GPU path failed (it is then latched
/// off for the session).
pub fn render(edit: &EditParams, seed: u32) -> Option<(HtmlCanvasElement, u32, u32)> {
    let _span = perf::Span::new("gpu.render");
    let res = STATE.with(|s| -> Result<(HtmlCanvasElement, u32, u32), String> {
        let mut st = s.borrow_mut();
        let (w, h) = {
            let (_, w, h) = st.base.as_ref().ok_or_else(|| "no base".to_string())?;
            (*w, *h)
        };
        let gpu = st.gpu.as_mut().ok_or_else(|| "gpu gone".to_string())?;
        gpu.draw(edit, seed, w, h)?;
        Ok((gpu.canvas.clone(), w, h))
    });
    match res {
        Ok(v) => Some(v),
        Err(e) => {
            warn(&format!("gpu render failed: {e}"));
            latch_off();
            None
        }
    }
}

fn warn(msg: &str) {
    web_sys::console::warn_1(&JsValue::from_str(msg));
}

struct Uniforms {
    map: HashMap<&'static str, WebGlUniformLocation>,
}

impl Uniforms {
    fn get(&self, name: &str) -> Option<&WebGlUniformLocation> {
        self.map.get(name)
    }
}

struct Gpu {
    canvas: HtmlCanvasElement,
    gl: Gl,
    program: WebGlProgram,
    vao: WebGlVertexArrayObject,
    base: WebGlTexture,
    base_dims: Option<(u32, u32)>,
    lut: WebGlTexture,
    tex_a: WebGlTexture,
    tex_b: WebGlTexture,
    fbo_a: WebGlFramebuffer,
    fbo_b: WebGlFramebuffer,
    targets: Option<(u32, u32)>,
    u: Uniforms,
}

const VS_SRC: &str = r#"#version 300 es
layout(location = 0) in vec2 a_pos;
void main() { gl_Position = vec4(a_pos, 0.0, 1.0); }
"#;

// Mirrors pixel/src/{rawops,color,noise,blur}.rs stage for stage. Every
// quantisation point is explicit so the output matches the CPU oracle:
// tone -> u8, LUT -> u8, adjust -> u8 (truncation), grain -> u8 (round).
const FS_SRC: &str = r#"#version 300 es
precision highp float;
precision highp int;

uniform sampler2D u_tex;
uniform sampler2D u_lut;
uniform vec3  u_gains;
uniform float u_exp;
uniform float u_hl;
uniform float u_sh;
uniform float u_nr;
uniform vec4  u_adj;
uniform float u_grain;
uniform float u_grain_gauss;
uniform float u_grain_mono;
uniform uint  u_seed;
uniform vec2  u_size;
uniform int   u_pass;
uniform int   u_rad;
uniform float u_k[49];

out vec4 o_col;

const float HL_KNEE = 0.6;
const float TAU = 6.2831855;

float srgb_to_linear(float v) {
    return v <= 0.04045 ? v / 12.92 : pow((v + 0.055) / 1.055, 2.4);
}

float linear_to_srgb(float v) {
    return v <= 0.0031308 ? v * 12.92 : 1.055 * pow(v, 1.0 / 2.4) - 0.055;
}

float tone_luma(vec3 c) { return dot(c, vec3(0.2126, 0.7152, 0.0722)); }

// rawops::apply's encode: floor(v*1024), 1025-entry table, round to u8.
float enc(float v) {
    float x = floor(clamp(v, 0.0, 1.0) * 1024.0);
    return floor(linear_to_srgb(x / 1024.0) * 255.0 + 0.5);
}

vec3 tone(vec3 c) {
    vec3 n = c / 255.0;
    vec3 rgb = vec3(srgb_to_linear(n.r), srgb_to_linear(n.g), srgb_to_linear(n.b));
    rgb *= u_gains * u_exp;
    float y = tone_luma(rgb);
    if (u_hl > 0.0 && y > HL_KNEE) {
        float head = 1.0 - HL_KNEE;
        float over = y - HL_KNEE;
        float compressed = HL_KNEE + head * (1.0 - exp(-over / head));
        float y2 = y + (compressed - y) * u_hl;
        rgb *= y2 / y;
        y = y2;
    }
    if (u_sh > 0.0) {
        float w = 1.0 - min(y / 0.18, 1.0);
        float y2 = y + u_sh * 0.04 * (w * w);
        if (y2 > y) {
            rgb *= y2 / max(y, 1e-6);
            y = y2;
        }
    }
    if (u_nr > 0.0) {
        float dark = 1.0 - min(y / 0.12, 1.0);
        float t = u_nr * dark * 0.8;
        if (t > 0.0) rgb = vec3(y) + (rgb - vec3(y)) * (1.0 - t);
    }
    return vec3(enc(rgb.r), enc(rgb.g), enc(rgb.b));
}

// Composed levels+curves tables, exactly `color::compose_tables`.
vec3 lut3(vec3 v) {
    return vec3(
        floor(texelFetch(u_lut, ivec2(int(v.r), 0), 0).r * 255.0 + 0.5),
        floor(texelFetch(u_lut, ivec2(int(v.g), 1), 0).r * 255.0 + 0.5),
        floor(texelFetch(u_lut, ivec2(int(v.b), 2), 0).r * 255.0 + 0.5));
}

// color::adjust_rgb on 0..255 values, truncated to u8 like `as u8`.
vec3 adjust(vec3 v) {
    vec3 r = (v - vec3(128.0)) * (1.0 + u_adj.y) + vec3(128.0 + u_adj.x * 255.0);
    float l = 0.299 * r.r + 0.587 * r.g + 0.114 * r.b;
    float s = 1.0 + u_adj.z;
    r = vec3(l) + (r - vec3(l)) * s;
    r.r += u_adj.w * 40.0;
    r.b -= u_adj.w * 40.0;
    return floor(clamp(r, 0.0, 255.0));
}

uint phash(uint x) {
    x ^= x >> 16u;
    x *= 0x7feb352du;
    x ^= x >> 15u;
    x *= 0x846ca68bu;
    x ^= x >> 16u;
    return x;
}

float punit(uint key) { return float(phash(key) >> 8u) * (1.0 / 16777216.0); }

float grain_at(uint idx, uint c) {
    if (u_grain <= 0.0) return 0.0;
    float spread = u_grain / 100.0 * 127.5;
    uint base = phash(u_seed ^ phash(idx));
    uint key = u_grain_mono > 0.5 ? base : base + c * 0x9e3779b9u;
    if (u_grain_gauss > 0.5) {
        float u1 = punit(key);
        float u2 = punit(key ^ 0x68e31da4u);
        return sqrt(-2.0 * log(1.0 - u1)) * cos(TAU * u2) * spread * (2.0 / 3.0);
    }
    return (punit(key) * 2.0 - 1.0) * spread;
}

vec3 sample_u8(sampler2D tex, vec2 uv) {
    return floor(texture(tex, uv).rgb * 255.0 + 0.5);
}

void main() {
    ivec2 p = ivec2(gl_FragCoord.xy);
    vec2 uv = gl_FragCoord.xy / u_size;

    if (u_pass == 2 || u_pass == 3) {
        vec3 acc = vec3(0.0);
        int wmax = int(u_size.x) - 1;
        int hmax = int(u_size.y) - 1;
        for (int i = -u_rad; i <= u_rad; i++) {
            int sx = clamp(p.x + (u_pass == 2 ? i : 0), 0, wmax);
            int sy = clamp(p.y + (u_pass == 3 ? i : 0), 0, hmax);
            acc += floor(texelFetch(u_tex, ivec2(sx, sy), 0).rgb * 255.0 + 0.5) * u_k[i + 24];
        }
        vec3 outv = floor(clamp(acc, 0.0, 255.0) + 0.5);
        o_col = vec4(outv / 255.0, 1.0);
        return;
    }

    vec3 t;
    if (u_pass == 1) {
        // Base is uploaded top-down; flip so canvas top = CPU row 0.
        t = tone(sample_u8(u_tex, vec2(uv.x, 1.0 - uv.y)));
        o_col = vec4(t / 255.0, 1.0);
        return;
    }
    if (u_pass == 4) {
        t = floor(texelFetch(u_tex, p, 0).rgb * 255.0 + 0.5);
    } else {
        t = tone(sample_u8(u_tex, vec2(uv.x, 1.0 - uv.y)));
    }

    vec3 v = adjust(lut3(t));
    uint idx = uint((int(u_size.y) - 1 - p.y) * int(u_size.x) + p.x);
    vec3 g = vec3(grain_at(idx, 0u), grain_at(idx, 1u), grain_at(idx, 2u));
    vec3 outv = floor(clamp(v + g, 0.0, 255.0) + 0.5);
    o_col = vec4(outv / 255.0, 1.0);
}
"#;

impl Gpu {
    fn new(force: bool) -> Result<Gpu, String> {
        let window = web_sys::window().ok_or("no window")?;
        let doc = window.document().ok_or("no document")?;
        let canvas = doc
            .create_element("canvas")
            .map_err(|_| "canvas element".to_string())?
            .dyn_into::<HtmlCanvasElement>()
            .map_err(|_| "canvas cast".to_string())?;

        let opts = Object::new();
        for (k, v) in [
            ("alpha", false),
            ("antialias", false),
            ("depth", false),
            ("stencil", false),
            ("premultipliedAlpha", false),
            ("preserveDrawingBuffer", false),
        ] {
            let _ = Reflect::set(&opts, &JsValue::from_str(k), &JsValue::from_bool(v));
        }
        // Auto mode asks Chromium to refuse a software-backed context
        // outright; the renderer-name check below catches browsers that
        // ignore the hint. Forced mode (tests) opts back in.
        let _ = Reflect::set(
            &opts,
            &JsValue::from_str("failIfMajorPerformanceCaveat"),
            &JsValue::from_bool(!force),
        );
        let gl = canvas
            .get_context_with_context_options("webgl2", &opts.into())
            .map_err(|_| "webgl2 context".to_string())?
            .ok_or("webgl2 unavailable")?
            .dyn_into::<Gl>()
            .map_err(|_| "webgl2 cast".to_string())?;

        if !force && !hardware_renderer(&gl) {
            return Err("software renderer".into());
        }

        let vs = compile(&gl, Gl::VERTEX_SHADER, VS_SRC)?;
        let fs = compile(&gl, Gl::FRAGMENT_SHADER, FS_SRC)?;
        let program = gl.create_program().ok_or("create_program")?;
        gl.attach_shader(&program, &vs);
        gl.attach_shader(&program, &fs);
        gl.link_program(&program);
        if gl
            .get_program_parameter(&program, Gl::LINK_STATUS)
            .as_bool()
            != Some(true)
        {
            let log = gl.get_program_info_log(&program).unwrap_or_default();
            return Err(format!("link: {log}"));
        }
        gl.detach_shader(&program, &vs);
        gl.detach_shader(&program, &fs);
        gl.delete_shader(Some(&vs));
        gl.delete_shader(Some(&fs));

        let vao = gl.create_vertex_array().ok_or("create_vao")?;
        gl.bind_vertex_array(Some(&vao));
        let buf = gl.create_buffer().ok_or("create_buffer")?;
        gl.bind_buffer(Gl::ARRAY_BUFFER, Some(&buf));
        let verts: [f32; 6] = [-1.0, -1.0, 3.0, -1.0, -1.0, 3.0];
        let mut bytes = Vec::with_capacity(24);
        for v in verts {
            bytes.extend_from_slice(&v.to_le_bytes());
        }
        gl.buffer_data_with_u8_array(Gl::ARRAY_BUFFER, &bytes, Gl::STATIC_DRAW);
        gl.enable_vertex_attrib_array(0);
        gl.vertex_attrib_pointer_with_i32(0, 2, Gl::FLOAT, false, 0, 0);

        let base = make_texture(&gl)?;
        gl.bind_texture(Gl::TEXTURE_2D, Some(&base));
        gl.tex_image_2d_with_i32_and_i32_and_i32_and_format_and_type_and_opt_u8_array(
            Gl::TEXTURE_2D,
            0,
            Gl::RGBA8 as i32,
            1,
            1,
            0,
            Gl::RGBA,
            Gl::UNSIGNED_BYTE,
            Some(&[0, 0, 0, 255]),
        )
        .map_err(|_| "base alloc".to_string())?;

        let lut = make_texture(&gl)?;
        gl.bind_texture(Gl::TEXTURE_2D, Some(&lut));
        gl.tex_parameteri(Gl::TEXTURE_2D, Gl::TEXTURE_MIN_FILTER, Gl::NEAREST as i32);
        gl.tex_parameteri(Gl::TEXTURE_2D, Gl::TEXTURE_MAG_FILTER, Gl::NEAREST as i32);
        gl.tex_image_2d_with_i32_and_i32_and_i32_and_format_and_type_and_opt_u8_array(
            Gl::TEXTURE_2D,
            0,
            Gl::R8 as i32,
            256,
            3,
            0,
            Gl::RED,
            Gl::UNSIGNED_BYTE,
            None,
        )
        .map_err(|_| "lut alloc".to_string())?;

        let tex_a = make_texture(&gl)?;
        let tex_b = make_texture(&gl)?;
        let fbo_a = gl.create_framebuffer().ok_or("create_fbo")?;
        let fbo_b = gl.create_framebuffer().ok_or("create_fbo")?;

        let mut map = HashMap::new();
        for name in [
            "u_tex", "u_lut", "u_gains", "u_exp", "u_hl", "u_sh", "u_nr", "u_adj",
            "u_grain", "u_grain_gauss", "u_grain_mono", "u_seed", "u_size", "u_pass",
            "u_rad", "u_k",
        ] {
            if let Some(l) = gl.get_uniform_location(&program, name) {
                map.insert(name, l);
            }
        }
        let u = Uniforms { map };
        gl.use_program(Some(&program));
        if let Some(l) = u.get("u_tex") {
            gl.uniform1i(Some(l), 0);
        }
        if let Some(l) = u.get("u_lut") {
            gl.uniform1i(Some(l), 1);
        }

        Ok(Gpu {
            canvas,
            gl,
            program,
            vao,
            base,
            base_dims: None,
            lut,
            tex_a,
            tex_b,
            fbo_a,
            fbo_b,
            targets: None,
            u,
        })
    }

    fn upload_base(&mut self, pixels: &[u8], w: u32, h: u32) -> Result<(), String> {
        let gl = &self.gl;
        gl.active_texture(Gl::TEXTURE0);
        gl.bind_texture(Gl::TEXTURE_2D, Some(&self.base));
        gl.tex_image_2d_with_i32_and_i32_and_i32_and_format_and_type_and_opt_u8_array(
            Gl::TEXTURE_2D,
            0,
            Gl::RGBA8 as i32,
            w as i32,
            h as i32,
            0,
            Gl::RGBA,
            Gl::UNSIGNED_BYTE,
            Some(pixels),
        )
        .map_err(|_| "base upload".to_string())?;
        self.base_dims = Some((w, h));
        Ok(())
    }

    fn ensure_targets(&mut self, w: u32, h: u32) -> Result<(), String> {
        if self.targets == Some((w, h)) {
            return Ok(());
        }
        let gl = &self.gl;
        for (tex, fbo) in [(&self.tex_a, &self.fbo_a), (&self.tex_b, &self.fbo_b)] {
            gl.bind_texture(Gl::TEXTURE_2D, Some(tex));
            gl.tex_image_2d_with_i32_and_i32_and_i32_and_format_and_type_and_opt_u8_array(
                Gl::TEXTURE_2D,
                0,
                Gl::RGBA8 as i32,
                w as i32,
                h as i32,
                0,
                Gl::RGBA,
                Gl::UNSIGNED_BYTE,
                None,
            )
            .map_err(|_| "target alloc".to_string())?;
            gl.bind_framebuffer(Gl::FRAMEBUFFER, Some(fbo));
            gl.framebuffer_texture_2d(
                Gl::FRAMEBUFFER,
                Gl::COLOR_ATTACHMENT0,
                Gl::TEXTURE_2D,
                Some(tex),
                0,
            );
            if gl.check_framebuffer_status(Gl::FRAMEBUFFER) != Gl::FRAMEBUFFER_COMPLETE {
                return Err("framebuffer incomplete".into());
            }
        }
        gl.bind_framebuffer(Gl::FRAMEBUFFER, None);
        self.targets = Some((w, h));
        Ok(())
    }

    fn draw(&mut self, edit: &EditParams, seed: u32, w: u32, h: u32) -> Result<(), String> {
        if self.canvas.width() != w {
            self.canvas.set_width(w);
        }
        if self.canvas.height() != h {
            self.canvas.set_height(h);
        }
        self.ensure_targets(w, h)?;

        let gl = &self.gl;
        if gl.is_context_lost() {
            return Err("context lost".into());
        }
        gl.viewport(0, 0, w as i32, h as i32);
        gl.use_program(Some(&self.program));
        gl.bind_vertex_array(Some(&self.vao));

        let l = |n: &str| self.u.get(n);
        let gains = rawops::wb_gains(edit.raw.temp, edit.raw.tint);
        u3f(gl, l("u_gains"), gains[0], gains[1], gains[2]);
        u1f(gl, l("u_exp"), 2f32.powf(edit.raw.exposure.clamp(-5.0, 5.0)));
        u1f(gl, l("u_hl"), (edit.raw.highlights / 100.0).clamp(0.0, 1.0));
        u1f(gl, l("u_sh"), (edit.raw.shadows / 100.0).clamp(0.0, 1.0));
        u1f(gl, l("u_nr"), (edit.raw.noise / 100.0).clamp(0.0, 1.0));
        u4f(
            gl,
            l("u_adj"),
            edit.brightness,
            edit.contrast,
            edit.saturation,
            edit.warmth,
        );
        u1f(gl, l("u_grain"), edit.grain);
        u1f(gl, l("u_grain_gauss"), if edit.grain_gaussian { 1.0 } else { 0.0 });
        u1f(gl, l("u_grain_mono"), if edit.grain_mono { 1.0 } else { 0.0 });
        if let Some(loc) = l("u_seed") {
            gl.uniform1ui(Some(loc), seed);
        }
        u2f(gl, l("u_size"), w as f32, h as f32);

        let tables = color::compose_tables(&edit.levels, &edit.curves);
        let mut lut = [0u8; 768];
        for (ch, table) in tables.iter().enumerate() {
            lut[ch * 256..(ch + 1) * 256].copy_from_slice(table);
        }
        gl.active_texture(Gl::TEXTURE1);
        gl.bind_texture(Gl::TEXTURE_2D, Some(&self.lut));
        gl.tex_image_2d_with_i32_and_i32_and_i32_and_format_and_type_and_opt_u8_array(
            Gl::TEXTURE_2D,
            0,
            Gl::R8 as i32,
            256,
            3,
            0,
            Gl::RED,
            Gl::UNSIGNED_BYTE,
            Some(&lut),
        )
        .map_err(|_| "lut upload".to_string())?;
        gl.active_texture(Gl::TEXTURE0);

        let blur_on = edit.blur > 0.0;
        let mut kbuf = [0f32; 49];
        let mut rad = 0i32;
        if blur_on {
            let sigma = edit.blur / 100.0 * 8.0;
            rad = ((sigma * 3.0).ceil() as i32).clamp(1, 24);
            let mut sum = 0.0f32;
            for i in -rad..=rad {
                let v = (-0.5 * (i as f32 / sigma).powi(2)).exp();
                kbuf[(i + 24) as usize] = v;
                sum += v;
            }
            if sum > 0.0 {
                for i in -rad..=rad {
                    kbuf[(i + 24) as usize] /= sum;
                }
            }
        }
        if let Some(loc) = l("u_k") {
            gl.uniform1fv_with_f32_array(Some(loc), &kbuf);
        }
        u1i(gl, l("u_rad"), rad);

        if blur_on {
            gl.bind_framebuffer(Gl::FRAMEBUFFER, Some(&self.fbo_a));
            gl.bind_texture(Gl::TEXTURE_2D, Some(&self.base));
            u1i(gl, l("u_pass"), 1);
            gl.draw_arrays(Gl::TRIANGLES, 0, 3);

            gl.bind_framebuffer(Gl::FRAMEBUFFER, Some(&self.fbo_b));
            gl.bind_texture(Gl::TEXTURE_2D, Some(&self.tex_a));
            u1i(gl, l("u_pass"), 2);
            gl.draw_arrays(Gl::TRIANGLES, 0, 3);

            gl.bind_framebuffer(Gl::FRAMEBUFFER, Some(&self.fbo_a));
            gl.bind_texture(Gl::TEXTURE_2D, Some(&self.tex_b));
            u1i(gl, l("u_pass"), 3);
            gl.draw_arrays(Gl::TRIANGLES, 0, 3);

            gl.bind_framebuffer(Gl::FRAMEBUFFER, None);
            gl.bind_texture(Gl::TEXTURE_2D, Some(&self.tex_a));
            u1i(gl, l("u_pass"), 4);
            gl.draw_arrays(Gl::TRIANGLES, 0, 3);
        } else {
            gl.bind_framebuffer(Gl::FRAMEBUFFER, None);
            gl.bind_texture(Gl::TEXTURE_2D, Some(&self.base));
            u1i(gl, l("u_pass"), 0);
            gl.draw_arrays(Gl::TRIANGLES, 0, 3);
        }
        Ok(())
    }
}

fn make_texture(gl: &Gl) -> Result<WebGlTexture, String> {
    let tex = gl.create_texture().ok_or("create_texture")?;
    gl.bind_texture(Gl::TEXTURE_2D, Some(&tex));
    gl.tex_parameteri(Gl::TEXTURE_2D, Gl::TEXTURE_MIN_FILTER, Gl::LINEAR as i32);
    gl.tex_parameteri(Gl::TEXTURE_2D, Gl::TEXTURE_MAG_FILTER, Gl::LINEAR as i32);
    gl.tex_parameteri(Gl::TEXTURE_2D, Gl::TEXTURE_WRAP_S, Gl::CLAMP_TO_EDGE as i32);
    gl.tex_parameteri(Gl::TEXTURE_2D, Gl::TEXTURE_WRAP_T, Gl::CLAMP_TO_EDGE as i32);
    Ok(tex)
}

fn compile(gl: &Gl, kind: u32, src: &str) -> Result<WebGlShader, String> {
    let sh = gl.create_shader(kind).ok_or("create_shader")?;
    gl.shader_source(&sh, src);
    gl.compile_shader(&sh);
    if gl.get_shader_parameter(&sh, Gl::COMPILE_STATUS).as_bool() != Some(true) {
        let log = gl.get_shader_info_log(&sh).unwrap_or_default();
        return Err(format!("shader: {log}"));
    }
    Ok(sh)
}

fn hardware_renderer(gl: &Gl) -> bool {
    let mut name = String::new();
    if let Ok(Some(ext)) = gl.get_extension("WEBGL_debug_renderer_info") {
        // The extension exposes the pname as a number; feed it to
        // get_parameter. (Reading the property itself yields the enum.)
        if let Ok(v) = Reflect::get(&ext, &JsValue::from_str("UNMASKED_RENDERER_WEBGL")) {
            if let Some(id) = v.as_f64() {
                if let Ok(p) = gl.get_parameter(id as u32) {
                    if let Some(n) = p.as_string() {
                        name = n;
                    }
                }
            }
        }
    }
    if name.is_empty() {
        if let Ok(v) = gl.get_parameter(Gl::RENDERER) {
            name = v.as_string().unwrap_or_default();
        }
    }
    let n = name.to_ascii_lowercase();
    if n.is_empty() {
        // Unknown: assume hardware. Forced mode bypasses this anyway.
        return true;
    }
    !(n.contains("swiftshader")
        || n.contains("llvmpipe")
        || n.contains("swangle")
        || n.contains("software")
        || n.contains("basic render driver"))
}

fn u1f(gl: &Gl, l: Option<&WebGlUniformLocation>, v: f32) {
    if let Some(l) = l {
        gl.uniform1f(Some(l), v);
    }
}

fn u1i(gl: &Gl, l: Option<&WebGlUniformLocation>, v: i32) {
    if let Some(l) = l {
        gl.uniform1i(Some(l), v);
    }
}

fn u2f(gl: &Gl, l: Option<&WebGlUniformLocation>, x: f32, y: f32) {
    if let Some(l) = l {
        gl.uniform2f(Some(l), x, y);
    }
}

fn u3f(gl: &Gl, l: Option<&WebGlUniformLocation>, x: f32, y: f32, z: f32) {
    if let Some(l) = l {
        gl.uniform3f(Some(l), x, y, z);
    }
}

fn u4f(gl: &Gl, l: Option<&WebGlUniformLocation>, x: f32, y: f32, z: f32, w: f32) {
    if let Some(l) = l {
        gl.uniform4f(Some(l), x, y, z, w);
    }
}
