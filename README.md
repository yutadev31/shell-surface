# shell-surface

`shell-surface` は、GUI シェル用 Rust ライブラリです。
Wayland の `wlr-layer-shell` と X11 の両方で、通知、ランチャー、オーバーレイ、パネルなどの surface を実装できます。

アプリケーションは `SurfaceConfig` の一覧と `Shell` trait を実装し、backend を選んで実行します。

```rust
use shell_surface::{Backend, InputEvent, Shell, Size, SurfaceConfig, SurfaceId};

struct App {
    surfaces: Vec<SurfaceConfig>,
}

impl Shell for App {
    fn surface_configs(&self) -> &[SurfaceConfig] {
        &self.surfaces
    }

    fn render(
        &mut self,
        _surface: SurfaceId,
        size: Size,
        _output: Option<&str>,
    ) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
        Ok(vec![0; size.width as usize * size.height as usize * 4])
    }

    fn handle_event(&mut self, _surface: SurfaceId, _event: InputEvent) {}
}

let mut app = App {
    surfaces: vec![SurfaceConfig::new("launcher", Size::new(800, 600))],
};
let mut backend = shell_surface::backend::wayland::WaylandBackend;
backend.run(&mut app)?;
# Ok::<(), Box<dyn std::error::Error>>(())
```

`SurfaceConfig::new` は compositor が配置する 1 surface を作ります。パネルのように全 output に作る場合は `output` を `OutputSelection::All` にし、`anchors` と `exclusive_zone` を設定します。

デフォルト feature は `wayland` と `x11` です。片方だけ使う場合は `--no-default-features --features wayland` または `x11` を指定できます。
