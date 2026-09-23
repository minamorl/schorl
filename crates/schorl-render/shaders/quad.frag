#version 450

// 貼るだけ。背景の黒は clear が持つのでここでは触らない。
// alpha は 1 に固める: 空間の背景は不透明の黒であり、窓の中身を透かさない。

layout(set = 0, binding = 0) uniform sampler2D surface;

layout(location = 0) in vec2 v_uv;
layout(location = 0) out vec4 out_color;

void main() {
    out_color = vec4(texture(surface, v_uv).rgb, 1.0);
}
