#version 450

// 貼るだけ。背景の黒は clear が持つのでここでは触らない。
// alpha は 1 に固める: 空間の背景は不透明の黒であり、窓の中身を透かさない。

layout(set = 0, binding = 0) uniform sampler2D surface;

layout(push_constant) uniform Push {
    mat4 mvp;
    vec4 cursor;
} push;

layout(location = 0) in vec2 v_uv;
layout(location = 0) out vec4 out_color;

void main() {
    vec3 colour = texture(surface, v_uv).rgb;
    if (push.cursor.z > 0.0 && push.cursor.w > 0.0) {
        float distance_from_cursor = length((v_uv - push.cursor.xy) / push.cursor.zw);
        if (distance_from_cursor <= 1.0) {
            // 白い外周と黒い芯は、明るい画面と暗い画面のどちらでも見失わない。
            colour = distance_from_cursor > 0.58 ? vec3(1.0) : vec3(0.0);
        }
    }
    out_color = vec4(colour, 1.0);
}
