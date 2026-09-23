# schorl

**状態: Quest 3 実機ではまだ確認していない (not yet verified on a physical Quest 3)。**

Linux 向けの自作 VR 作業環境。真っ暗な 360 度の VR 空間に Linux のウィンドウを置いて作業する。
schorl 自身が Wayland compositor になり、toplevel を空間へ直接置く
(`pins/domains/schorl.spec@0.4`)。名前は仮置き。

0.1 の「Linux のディスプレイを一枚 capture して板に貼る」構成は原文3 で v1 から
外れた。`schorl-capture` / `schorl-display` / `schorl-panel-driver` はその経路の
実測資産として workspace に残してあるが、v1 のバイナリはどれも通らない。

## 建てる

```
cargo build --workspace
```

shader は GLSL の source (`crates/schorl-render/shaders/*.vert`, `*.frag`) だけを
repo に置き、SPIR-V は build script が `OUT_DIR` へ焼く (`pin public.no_build_artifacts`)。
そのため生成器が要る。

```
apt install glslang-tools   # glslangValidator。必須
apt install spirv-tools     # spirv-val。焼いた物の検証に使う。任意
```

無ければ build は黙って壊れず、何を探したかと入れ方を書いて止まる。別の場所の物を
使うなら `SCHORL_GLSL_COMPILER` / `SCHORL_SPIRV_VAL` で名指しする
(`glslc` でも焼ける)。

## 走らせる

`schorl` は一本のプログラムである。自分の Wayland ソケットを取り、繋いできた
toplevel を 360 度の空間へ置き、OpenXR の swapchain へ出すところまでを同じ
プロセスでやる。

```
schorl [client argv…]
```

引数を与えるとそのクライアントを schorl のソケットへ向けて起こす。与えなければ
ソケット名を出して待つので、別の端末から `WAYLAND_DISPLAY=<その名前> <client>` で
繋ぐ。宿主の compositor のセッションは置き換えない
(`pin wm.host_compositor_coexistence`)。

## HMD 無しの確認

```
scripts/schorl-hmdless-check.sh
```

`monado-service` を起こし、`schorl-seam-check` を走らせ、起こした物を返す。
検査が見るのは三つで、どれも同じ走りの中に較正を持っている。

1. **クライアントの画素が swapchain まで乗ったか (`wl_shm` 経路)。** 外のプロセス
   (`schorl-probe-client`) が 840 通りから四分割の並びを一つ選び、描く前に
   標準出力へ申告する。検査は申告を読み、swapchain の view 0 を読み戻して
   照合する。較正は (a) クライアントが繋がる前の一枚は照合が通らないこと、
   (b) 同じ照合器へ申告と違う並びを通すとその違う並びが返ること。
2. **コントローラで toplevel を掴んで置き直せたか。** `schorl-panel` の掴みの
   算術を通し、台帳の姿勢が動き、読み戻した絵の中で板が動くところまで。
3. **同じことが dmabuf 経路でも起きるか。** 1 のクライアントを降ろしてから
   `schorl-probe-client-dmabuf` を立てる。こちらは `wl_shm` を bind せず、
   GPU 上の `VkImage` を `zwp_linux_dmabuf_v1` で渡す。**退路が無い。**
   較正は (c) クライアントが降りた直後の一枚は照合が通らないこと。加えて
   台帳の `dmabuf_draws` が増え `shm_draws` が一つも増えていないことを見るので、
   読めた絵が退路から来たものではありえない。この経路には export できる
   Vulkan (`VK_EXT_external_memory_dma_buf` / `VK_EXT_image_drm_format_modifier`)
   が要る。

`SCHORL_SEAM_DUMP=<path>` を与えると、ランタイムへ渡した絵を PPM で書き出す。

**これは受け入れではない。** Quest 3 を被っての確認は御主人様の身体が要る
(`pin verify.hmd_gate` / `pin verify.no_green_substitute`)。検査が最後に出す
`hmd_accepted` は必ず `unknown` であり、掴みボタンを押したのが人でないことは
`synthetic_controller_events` の欄で申告される。

## ライセンス

MIT OR Apache-2.0 の dual。[LICENSE-MIT](LICENSE-MIT) と
[LICENSE-APACHE](LICENSE-APACHE) のどちらかを選べる ([LICENSE](LICENSE))。
package registry (crates.io 等) へは出さない (`pin release.no_registry_publish`、
workspace の `publish = false`)。
