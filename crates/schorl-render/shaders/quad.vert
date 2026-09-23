#version 450

// 矩形のサーフェスを一枚描く。頂点バッファを持たず、strip の 4 隅を
// gl_VertexIndex から引く。サーフェスの寸法と姿勢は mvp に畳んである。
//
// ローカル平面は schorl_panel の板と同じ取り方: x が右、y が上、z = 0。

layout(push_constant) uniform Push {
    mat4 mvp;
} push;

layout(location = 0) out vec2 v_uv;

void main() {
    // 0:(-x,-y) 1:(+x,-y) 2:(-x,+y) 3:(+x,+y) — triangle strip で 2 枚。
    float x = (gl_VertexIndex == 1 || gl_VertexIndex == 3) ? 0.5 : -0.5;
    float y = (gl_VertexIndex >= 2) ? 0.5 : -0.5;
    // 画像の原点は左上。平面の +y は上なので v を反転して貼る。
    v_uv = vec2(x + 0.5, 0.5 - y);
    gl_Position = push.mvp * vec4(x, y, 0.0, 1.0);
}
