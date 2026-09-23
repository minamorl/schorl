# schorl

Linux 向けの自作 VR 作業環境。真っ暗な 360 度の VR 空間に Linux のウィンドウを置いて作業する。
schorl 自身が Wayland compositor になり、toplevel を空間へ直接置く
(`pins/domains/schorl.spec@0.2`)。名前は仮置き。

0.1 の「Linux のディスプレイを一枚 capture して板に貼る」構成は原文3 で v1 から
外れた。`schorl-capture` / `schorl-display` / `schorl-panel-driver` はその経路の
実測資産として workspace に残してあるが、v1 のバイナリはどれも通らない。
