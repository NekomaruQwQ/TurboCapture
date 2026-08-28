/**
 * GPU-accelerated color-key renderer.
 *
 * Replaces pixels matching one of N target colors with transparency using a
 * WebGL2 fragment shader.  The entire pipeline stays on the GPU — the video
 * frame is uploaded as a texture and the shader runs per-pixel.
 *
 * Algorithm (per pixel, in linear-light space):
 *   1. Convert the source pixel sRGB → linear.
 *   2. For each key, estimate per-channel "foreground signal vs. background"
 *      ratio and take the max-channel.  Pick the lowest result across keys —
 *      that's the alpha estimate (transparent if it matches *any* key) and
 *      identifies the best-matching key for unspill.
 *   3. Shape with `smoothstep(kneeLow, kneeHigh, alpha)` — clean noise floor,
 *      snap near-solid to 1, preserve anti-aliased edges in between.
 *   4. Unspill against the best-matching key, divide out alpha to produce
 *      straight (non-premultiplied) RGB, re-encode linear → sRGB.
 *
 * Working in linear space is what kills the dark fringes you'd otherwise get
 * doing this naively in sRGB.
 */

// ── Shaders ──────────────────────────────────────────────────────────────────

/// Fullscreen triangle from gl_VertexID — no vertex buffer needed.
const VERT_SRC = /* glsl */ `#version 300 es
out vec2 v_uv;
void main() {
    // Vertices: (-1,-1), (3,-1), (-1,3) — covers the full clip quad.
    vec2 pos = vec2(
        float((gl_VertexID & 1) << 2) - 1.0,
        float((gl_VertexID & 2) << 1) - 1.0);
    v_uv = pos * 0.5 + 0.5;
    v_uv.y = 1.0 - v_uv.y;  // flip Y for video texture coordinates
    gl_Position = vec4(pos, 0.0, 1.0);
}
`

/// Maximum number of simultaneous key colors the shader supports.
/// GLSL ES 3.00 requires array sizes to be compile-time constants, so this
/// is baked into the fragment shader and validated by the renderer.
export const MAX_KEYS = 8

/// Color-key fragment shader.  Key colors arrive pre-converted to linear
/// (CPU-side, see {@link ColorKeyRenderer}'s constructor) so the shader
/// avoids redoing sRGB→linear per-pixel for what are effectively constants.
/// Output is straight (non-premultiplied) alpha to match the WebGL context's
/// `premultipliedAlpha: false` attribute — the browser composites correctly
/// against whatever CSS background is behind the canvas.
const FRAG_SRC = /* glsl */ `#version 300 es
precision mediump float;
in vec2 v_uv;
out vec4 fragColor;
uniform sampler2D u_texture;
uniform vec3 u_keyColorsL[${MAX_KEYS}];  // pre-linearized key colors in [0,1]
uniform int u_keyCount;                   // active entries in u_keyColorsL
uniform float u_kneeLow;                  // smoothstep low edge  (e.g. 0.02)
uniform float u_kneeHigh;                 // smoothstep high edge (e.g. 0.98)
uniform int   u_useBinarization;          // 0 = normal output, !=0 = constant tint
uniform vec3  u_binarizationColor;        // sRGB straight, [0,1] — used iff gate non-zero

vec3 srgbToLinear(vec3 c) {
    return mix(c / 12.92,
               pow((c + 0.055) / 1.055, vec3(2.4)),
               step(0.04045, c));
}
vec3 linearToSrgb(vec3 c) {
    return mix(c * 12.92,
               1.055 * pow(max(c, 0.0), vec3(1.0/2.4)) - 0.055,
               step(0.0031308, c));
}

void main() {
    vec3 srcL = srgbToLinear(texture(u_texture, v_uv).rgb);

    // Best-match across keys: lowest per-key alpha = closest match.  Tracking
    // the index lets us unspill against the actual contaminating background
    // rather than picking arbitrarily.  Loop bound is compile-time MAX_KEYS so
    // the compiler can unroll; runtime count is enforced via early-out.
    float bestAlpha = 1.0;
    int   bestKey   = 0;
    for (int i = 0; i < ${MAX_KEYS}; ++i) {
        if (i >= u_keyCount) break;
        vec3  keyL = u_keyColorsL[i];
        vec3  norm = max(srcL - keyL, 0.0) / max(vec3(1.0) - keyL, vec3(1e-5));
        float a    = max(norm.r, max(norm.g, norm.b));
        if (a < bestAlpha) { bestAlpha = a; bestKey = i; }
    }

    // Soft knee: clean noise floor, snap near-solid to 1, preserve AA between.
    float alpha = smoothstep(u_kneeLow, u_kneeHigh, bestAlpha);

    // Binarization fast-path: skip unspill entirely and emit the constant
    // tint with the keyer's soft alpha.  Uniform branch is coherent across
    // the warp, so this costs nothing when the feature is off.
    if (u_useBinarization != 0) {
        fragColor = vec4(u_binarizationColor, alpha);
        return;
    }

    // Unspill against the best-matching key, then divide out alpha to recover
    // straight RGB.  The 1e-5 floor avoids div-by-zero; when alpha is tiny
    // the RGB doesn't contribute to compositing anyway.
    vec3 keyL   = u_keyColorsL[bestKey];
    vec3 premul = max(srcL - keyL * (1.0 - alpha), 0.0);
    vec3 rgbL   = premul / max(alpha, 1e-5);

    fragColor = vec4(linearToSrgb(rgbL), alpha);
    // fragColor = vec4(alpha, alpha, alpha, alpha);  // --- DEBUG: visualize alpha as grayscale ---
}
`

// ── Renderer ─────────────────────────────────────────────────────────────────

/// Default smoothstep knees over the unspill ratio in [0,1].  See the
/// algorithm overview at the top of this file for what each edge does.
export const DEFAULT_KNEE_LOW = 0.02
/// Default upper smoothstep edge, shared by the renderer and URL parser.
export const DEFAULT_KNEE_HIGH = 0.98

/// Color-key parameters that may change at runtime.  All fields can be
/// reassigned via {@link ColorKeyRenderer.updateParams} without rebuilding
/// the GL context, shader program, or texture.
export type ColorKeyParams = {
    /// RGB tuples in [0,255] to key out.  Empty = passthrough (the shader
    /// loop early-exits and alpha pegs to 1).  Capped at {@link MAX_KEYS}.
    keyColors: [number, number, number][]
    /// Smoothstep low edge.  Default {@link DEFAULT_KNEE_LOW}.
    kneeLow?: number
    /// Smoothstep high edge.  Default {@link DEFAULT_KNEE_HIGH}.
    kneeHigh?: number
    /// When set, the kept-pixel RGB is replaced by this constant sRGB
    /// color while the keyer's soft alpha is preserved.  Skips unspill.
    binarizationColor?: [number, number, number]
}

export class ColorKeyRenderer {
    private gl: WebGL2RenderingContext
    private program: WebGLProgram
    private texture: WebGLTexture
    private vao: WebGLVertexArrayObject
    private canvas: HTMLCanvasElement

    // ── Cached uniform locations ─────────────────────────────────────────
    // The program is immutable for the lifetime of this renderer, so we look
    // these up once at construction and reuse them on every `updateParams`.
    private uKeyColorsL: WebGLUniformLocation
    private uKeyCount: WebGLUniformLocation
    private uKneeLow: WebGLUniformLocation
    private uKneeHigh: WebGLUniformLocation
    private uUseBinarization: WebGLUniformLocation
    private uBinarizationColor: WebGLUniformLocation

    /**
     * @param canvas            Target canvas element (will be bound to a WebGL2 context).
     * @param keyColors         Initial key colors — see {@link ColorKeyParams.keyColors}.
     *                          Defaults to `[]` (passthrough); change at runtime via
     *                          {@link updateParams} without recreating the renderer.
     * @param kneeLow           Initial low knee — see {@link ColorKeyParams.kneeLow}.
     * @param kneeHigh          Initial high knee — see {@link ColorKeyParams.kneeHigh}.
     * @param binarizationColor Initial binarization tint — see
     *                          {@link ColorKeyParams.binarizationColor}.
     */
    constructor(
        canvas: HTMLCanvasElement,
        keyColors: [number, number, number][] = [],
        kneeLow = DEFAULT_KNEE_LOW,
        kneeHigh = DEFAULT_KNEE_HIGH,
        binarizationColor?: [number, number, number],
    ) {
        this.canvas = canvas

        const gl = canvas.getContext("webgl2", {
            alpha: true,
            premultipliedAlpha: false,
        })
        if (!gl) throw new Error("ColorKeyRenderer: WebGL2 not available")
        this.gl = gl

        // ── Compile & link ───────────────────────────────────────────────
        this.program = createProgram(gl, VERT_SRC, FRAG_SRC)

        // ── Empty VAO (vertex positions computed from gl_VertexID) ────────
        const vao = gl.createVertexArray()
        if (!vao) throw new Error("ColorKeyRenderer: failed to create VAO")
        this.vao = vao
        gl.bindVertexArray(this.vao)
        gl.bindVertexArray(null)

        // ── Texture for video frames ─────────────────────────────────────
        const texture = gl.createTexture()
        if (!texture) throw new Error("ColorKeyRenderer: failed to create texture")
        this.texture = texture
        gl.bindTexture(gl.TEXTURE_2D, this.texture)
        gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_S, gl.CLAMP_TO_EDGE)
        gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_T, gl.CLAMP_TO_EDGE)
        gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MIN_FILTER, gl.LINEAR)
        gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MAG_FILTER, gl.LINEAR)

        // ── Cache uniform locations & bind sampler ───────────────────────
        // u_texture is a sampler unit binding that never changes, so we set
        // it here and never touch it again.  Everything else is cached for
        // updateParams to push at runtime.
        gl.useProgram(this.program)
        gl.uniform1i(getUniform(gl, this.program, "u_texture"), 0)
        this.uKeyColorsL        = getUniform(gl, this.program, "u_keyColorsL")
        this.uKeyCount          = getUniform(gl, this.program, "u_keyCount")
        this.uKneeLow           = getUniform(gl, this.program, "u_kneeLow")
        this.uKneeHigh          = getUniform(gl, this.program, "u_kneeHigh")
        this.uUseBinarization   = getUniform(gl, this.program, "u_useBinarization")
        this.uBinarizationColor = getUniform(gl, this.program, "u_binarizationColor")

        this.updateParams({ keyColors, kneeLow, kneeHigh, binarizationColor })
        this.clear()
    }

    /**
     * Re-upload color-key uniforms.  Cheap (a handful of `gl.uniform*` calls);
     * intended to be called whenever the keying configuration changes without
     * rebuilding the renderer, GL context, or in-flight video pipeline.
     *
     * @throws if `keyColors.length > {@link MAX_KEYS}`.
     */
    updateParams(params: ColorKeyParams): void {
        const {
            keyColors,
            kneeLow = DEFAULT_KNEE_LOW,
            kneeHigh = DEFAULT_KNEE_HIGH,
            binarizationColor,
        } = params

        if (keyColors.length > MAX_KEYS)
            throw new Error(`ColorKeyRenderer: at most ${MAX_KEYS} key colors supported (got ${keyColors.length})`)

        const gl = this.gl
        gl.useProgram(this.program)

        // Pre-linearize keys on the CPU so the shader doesn't redo sRGB→linear
        // per-pixel for what are effectively constants.  Trailing slots stay
        // zero (they're gated out by u_keyCount), so we only upload the
        // populated prefix — the previous tail is harmless.
        const flat = new Float32Array(keyColors.length * 3)
        keyColors.forEach(([r, g, b], i) => {
            flat[i * 3 + 0] = srgbToLinear(r / 255)
            flat[i * 3 + 1] = srgbToLinear(g / 255)
            flat[i * 3 + 2] = srgbToLinear(b / 255)
        })

        if (flat.length > 0) gl.uniform3fv(this.uKeyColorsL, flat)
        gl.uniform1i(this.uKeyCount, keyColors.length)
        gl.uniform1f(this.uKneeLow, kneeLow)
        gl.uniform1f(this.uKneeHigh, kneeHigh)

        // Gate is set unconditionally so the shader branch is well-defined
        // when binarization is off; the color uniform only matters when the
        // gate is non-zero, so we skip uploading it in the off case.
        const useBin = binarizationColor !== undefined
        gl.uniform1i(this.uUseBinarization, useBin ? 1 : 0)
        if (useBin) {
            const [r, g, b] = binarizationColor
            gl.uniform3f(this.uBinarizationColor, r / 255, g / 255, b / 255)
        }
    }

    /** Render a decoded video frame with color-key applied. Closes the frame. */
    render(frame: VideoFrame): void {
        const gl = this.gl
        try {
            // Resize canvas + viewport when video dimensions change.
            if (this.canvas.width !== frame.displayWidth || this.canvas.height !== frame.displayHeight) {
                this.canvas.width = frame.displayWidth
                this.canvas.height = frame.displayHeight
                gl.viewport(0, 0, frame.displayWidth, frame.displayHeight)
                console.log("ColorKeyRenderer: Resized to %dx%d", frame.displayWidth, frame.displayHeight)
            }

            // Upload frame as texture.
            gl.bindTexture(gl.TEXTURE_2D, this.texture)
            gl.texImage2D(gl.TEXTURE_2D, 0, gl.RGBA, gl.RGBA, gl.UNSIGNED_BYTE, frame)

            // Draw fullscreen triangle after the upload has transferred frame ownership to GL.
            gl.useProgram(this.program)
            gl.bindVertexArray(this.vao)
            gl.drawArrays(gl.TRIANGLES, 0, 3)
        } finally {
            // Decoder surfaces are scarce, so failures must release the frame too.
            frame.close()
        }
    }

    /** Clears decoded pixels to fully transparent while waiting or disconnected. */
    clear(): void {
        const gl = this.gl
        gl.clearColor(0, 0, 0, 0)
        gl.clear(gl.COLOR_BUFFER_BIT)
    }

    /** Releases the renderer's GPU resources when the page is discarded. */
    dispose(): void {
        const gl = this.gl
        gl.deleteTexture(this.texture)
        gl.deleteVertexArray(this.vao)
        gl.deleteProgram(this.program)
    }
}

// ── WebGL helpers ────────────────────────────────────────────────────────────

/// Look up a uniform location and throw with a useful error if the shader's
/// optimizer dropped it (e.g. the uniform isn't actually referenced in the
/// program).  Cached results are non-null so the rest of the renderer can
/// treat them as plain `WebGLUniformLocation`.
function getUniform(gl: WebGL2RenderingContext, program: WebGLProgram, name: string): WebGLUniformLocation {
    const loc = gl.getUniformLocation(program, name)
    if (loc === null) throw new Error(`ColorKeyRenderer: uniform "${name}" not found`)
    return loc
}

function compileShader(gl: WebGL2RenderingContext, type: number, source: string): WebGLShader {
    const shader = gl.createShader(type)
    if (!shader) throw new Error(`Failed to create shader (type=${type})`)
    gl.shaderSource(shader, source)
    gl.compileShader(shader)
    if (!gl.getShaderParameter(shader, gl.COMPILE_STATUS)) {
        const log = gl.getShaderInfoLog(shader)
        gl.deleteShader(shader)
        throw new Error(`Shader compile error: ${log}`)
    }
    return shader
}

function createProgram(gl: WebGL2RenderingContext, vertSrc: string, fragSrc: string): WebGLProgram {
    const vert = compileShader(gl, gl.VERTEX_SHADER, vertSrc)
    const frag = compileShader(gl, gl.FRAGMENT_SHADER, fragSrc)
    const program = gl.createProgram()
    if (!program) throw new Error("Failed to create WebGL program")
    gl.attachShader(program, vert)
    gl.attachShader(program, frag)
    gl.linkProgram(program)
    if (!gl.getProgramParameter(program, gl.LINK_STATUS)) {
        const log = gl.getProgramInfoLog(program)
        gl.deleteProgram(program)
        throw new Error(`Program link error: ${log}`)
    }
    // Shaders are linked — no longer needed as standalone objects.
    gl.deleteShader(vert)
    gl.deleteShader(frag)
    return program
}

/// sRGB → linear-light conversion for a single component in [0,1].  Mirrors
/// the GLSL `srgbToLinear` so CPU-pre-converted key colors match what the
/// shader would compute if it ran the conversion itself.
function srgbToLinear(c: number): number {
    return c <= 0.04045 ? c / 12.92 : ((c + 0.055) / 1.055) ** 2.4
}

