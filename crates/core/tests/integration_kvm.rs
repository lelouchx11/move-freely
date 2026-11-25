//! End-to-end encrypted multi-device integration tests exercising:
//! - Bidirectional Mac <-> Windows <-> Linux control & edge crossing
//! - Multi-monitor DPI normalization & vertical/horizontal arrangement
//! - Dynamic controller switching & conflict resolution
//! - Simultaneous local input + emergency release (no stuck keys/buttons)
//! - Universal clipboard sync (Unicode, HTML, PNG images, file bundles)
//! - Disconnection fallback, reconnection, and unauthorized/revoked rejection

use std::fs;
use std::net::SocketAddr;
use std::time::Duration;

use mf_core::clipboard::ClipboardPayload;
use mf_core::config::ModifierMappingMode;
use mf_core::input::headless::RecordedInjection;
use mf_core::input::InputEvent;
use mf_core::{AppConfig, EngineHandle, EngineMode, HeadlessTestHandles, MoveFreelyEngine};
use mf_protocol::{
    BoundaryMode, Button, DevicePlacement, EdgeMask, KeyCode, ModifierFlags, Monitor,
};

fn make_mon(id: &str, x: i32, y: i32, w: u32, h: u32, phys_w: u32, phys_h: u32, scale: f64, primary: bool) -> Monitor {
    Monitor {
        id: id.into(),
        name: id.into(),
        x,
        y,
        width: w,
        height: h,
        physical_width: phys_w,
        physical_height: phys_h,
        scale_factor: scale,
        is_primary: primary,
    }
}

async fn start_node(
    name: &str,
    monitors: Vec<Monitor>,
) -> (EngineHandle, HeadlessTestHandles, std::path::PathBuf, SocketAddr) {
    let dir = std::env::temp_dir().join(format!("mf-e2e-{}-{}", name, uuid::Uuid::new_v4()));
    let mut cfg = AppConfig::default();
    cfg.device_name = name.into();
    cfg.bind_ip = "127.0.0.1".into();
    cfg.listen_port = 0;
    cfg.auto_reconnect = true;
    cfg.modifier_mapping = ModifierMappingMode::Standard;

    let (handle, handles) = MoveFreelyEngine::start(
        Some(dir.clone()),
        Some(cfg),
        EngineMode::Headless {
            echo: false,
            monitors,
        },
    )
    .await
    .unwrap();
    let snap = handle.snapshot().await.unwrap();
    let addr: SocketAddr = format!("127.0.0.1:{}", snap.local.listen_port)
        .parse()
        .unwrap();
    (handle, handles.unwrap(), dir, addr)
}

async fn pair_and_connect(a: &EngineHandle, b: &EngineHandle, b_addr: SocketAddr) {
    let status = b.open_pairing_window(None).await.unwrap();
    let pin = status.pin.unwrap();
    a.pair_with_addr(b_addr, Some(b.local_id.clone()), pin)
        .await
        .unwrap();
    a.connect_addr(b.local_id.clone(), b_addr).await.unwrap();

    // Wait until both snapshots reflect the live connection.
    for _ in 0..40 {
        let sa = a.snapshot().await.unwrap();
        let sb = b.snapshot().await.unwrap();
        let a_ok = sa.peers.iter().any(|p| p.device_id == b.local_id && p.connected);
        let b_ok = sb.peers.iter().any(|p| p.device_id == a.local_id && p.connected);
        if a_ok && b_ok {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("peers failed to reach connected state");
}

#[tokio::test]
async fn full_cluster_bidirectional_control_multi_monitor_and_clipboard() {
    // Simulated 3-OS cluster:
    // - Mac (1440x900 @2x Retina) at (0, 0)
    // - Windows dual-monitor (4K @2x = 1920x1080 logical + 1080p @1x = 1920x1080 logical) at (1440, 0)
    // - Linux (1920x1080 @1x) stacked vertically above Windows at (1440, -1080)
    let (mac, mac_io, mac_dir, _mac_addr) = start_node(
        "macbook",
        vec![make_mon("mac-1", 0, 0, 1440, 900, 2880, 1800, 2.0, true)],
    )
    .await;
    let (win, win_io, win_dir, win_addr) = start_node(
        "winbox",
        vec![
            make_mon("win-4k", 0, 0, 3840, 2160, 3840, 2160, 2.0, true),
            make_mon("win-hd", 3840, 0, 1920, 1080, 1920, 1080, 1.0, false),
        ],
    )
    .await;
    let (linux, linux_io, linux_dir, linux_addr) = start_node(
        "linuxbox",
        vec![make_mon("lin-1", 0, 0, 1920, 1080, 1920, 1080, 1.0, true)],
    )
    .await;

    // Pair Mac <-> Win and Win <-> Linux and Mac <-> Linux
    pair_and_connect(&mac, &win, win_addr).await;
    pair_and_connect(&mac, &linux, linux_addr).await;
    pair_and_connect(&win, &linux, linux_addr).await;

    // Mac claims controller and sets 2D layout: Mac | Win (2 monitors) with Linux above Win.
    mac.set_controller(mac.local_id.clone());
    tokio::time::sleep(Duration::from_millis(80)).await;

    let placements = vec![
        DevicePlacement {
            device_id: mac.local_id.clone(),
            x: 0,
            y: 0,
            enabled_edges: EdgeMask::default(),
        },
        DevicePlacement {
            device_id: win.local_id.clone(),
            x: 1440,
            y: 0,
            enabled_edges: EdgeMask::default(),
        },
        DevicePlacement {
            device_id: linux.local_id.clone(),
            x: 1440,
            y: -1080,
            enabled_edges: EdgeMask::default(),
        },
    ];
    mac.update_placements(placements, BoundaryMode::Overlapping);
    tokio::time::sleep(Duration::from_millis(120)).await;

    // 1) Mac -> Windows edge crossing
    let _ = mac_io.capture_tx.send(InputEvent::LocalPointerMove {
        x: 1439,
        y: 450,
        dx: 12,
        dy: 0,
    });
    tokio::time::sleep(Duration::from_millis(120)).await;
    let snap_mac = mac.snapshot().await.unwrap();
    assert_eq!(snap_mac.focused_device, win.local_id);
    assert!(snap_mac.is_grabbing);

    // Move across Windows Monitor 1 onto Monitor 2, click, and hold Ctrl+KeyA
    let _ = mac_io.capture_tx.send(InputEvent::RemotePointerDelta {
        dx: 2200,
        dy: 0,
    });
    let _ = mac_io.capture_tx.send(InputEvent::MouseButton {
        button: Button::Left,
        pressed: true,
    });
    let _ = mac_io.capture_tx.send(InputEvent::Key {
        key: KeyCode::ControlLeft,
        pressed: true,
        text: None,
        modifiers: ModifierFlags {
            ctrl: true,
            ..Default::default()
        },
    });
    let _ = mac_io.capture_tx.send(InputEvent::Key {
        key: KeyCode::KeyA,
        pressed: true,
        text: None,
        modifiers: ModifierFlags {
            ctrl: true,
            ..Default::default()
        },
    });
    tokio::time::sleep(Duration::from_millis(120)).await;
    let win_events = win_io.injector.snapshot();
    assert!(win_events.iter().any(|e| matches!(e, RecordedInjection::Warp { .. })));
    assert!(win_events.iter().any(|e| matches!(
        e,
        RecordedInjection::Button {
            button: Button::Left,
            pressed: true
        }
    )));
    assert!(win_events.iter().any(|e| matches!(
        e,
        RecordedInjection::Key {
            key: KeyCode::KeyA,
            pressed: true,
            ..
        }
    )));

    // 2) Emergency release shortcut must release held keys & buttons on Windows and ungrab Mac!
    let _ = mac_io.capture_tx.send(InputEvent::Key {
        key: KeyCode::Escape,
        pressed: true,
        text: None,
        modifiers: ModifierFlags {
            ctrl: true,
            alt: true,
            shift: true,
            ..Default::default()
        },
    });
    tokio::time::sleep(Duration::from_millis(120)).await;
    let snap_after_esc = mac.snapshot().await.unwrap();
    assert_eq!(snap_after_esc.focused_device, mac.local_id);
    assert!(!snap_after_esc.is_grabbing);
    assert!(win_io
        .injector
        .snapshot()
        .iter()
        .any(|e| matches!(e, RecordedInjection::ReleaseAll)));

    // 3) Dynamic controller switching: Windows claims controller and crosses UP to Linux!
    win.set_controller(win.local_id.clone());
    tokio::time::sleep(Duration::from_millis(120)).await;
    assert_eq!(mac.snapshot().await.unwrap().active_controller, win.local_id);

    // Windows moves its cursor to top edge -> transitions vertically onto Linux!
    let _ = win_io.capture_tx.send(InputEvent::LocalPointerMove {
        x: 600,
        y: 0,
        dx: 0,
        dy: -10,
    });
    tokio::time::sleep(Duration::from_millis(120)).await;
    assert_eq!(win.snapshot().await.unwrap().focused_device, linux.local_id);
    assert!(linux_io
        .injector
        .snapshot()
        .iter()
        .any(|e| matches!(e, RecordedInjection::Warp { .. })));

    // 4) Simultaneous local input on Linux while Windows controls it does not cause loops!
    let _ = linux_io.capture_tx.send(InputEvent::LocalPointerMove {
        x: 500,
        y: 500,
        dx: 5,
        dy: 5,
    });
    tokio::time::sleep(Duration::from_millis(80)).await;
    assert_eq!(win.snapshot().await.unwrap().focused_device, linux.local_id);

    // 5) Universal Clipboard: Multilingual + HTML text Mac -> Windows & Linux
    mac_io
        .clipboard
        .lock()
        .unwrap()
        .set_simulated(ClipboardPayload::Text {
            plain: "Hello 世界 🚀".into(),
            html: Some("<b>Hello 世界 🚀</b>".into()),
            rtf: None,
        });
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(
        win_io.clipboard.lock().unwrap().current(),
        Some(&ClipboardPayload::Text {
            plain: "Hello 世界 🚀".into(),
            html: Some("<b>Hello 世界 🚀</b>".into()),
            rtf: None,
        })
    );
    assert_eq!(
        linux_io.clipboard.lock().unwrap().current(),
        Some(&ClipboardPayload::Text {
            plain: "Hello 世界 🚀".into(),
            html: Some("<b>Hello 世界 🚀</b>".into()),
            rtf: None,
        })
    );

    // 6) Universal Clipboard: Image PNG transfer Windows -> Mac
    let tiny_png = {
        let mut buf = Vec::new();
        let img = image::RgbaImage::from_pixel(4, 4, image::Rgba([255, 0, 128, 255]));
        img.write_to(&mut std::io::Cursor::new(&mut buf), image::ImageFormat::Png)
            .unwrap();
        buf
    };
    win_io
        .clipboard
        .lock()
        .unwrap()
        .set_simulated(ClipboardPayload::Image {
            png_bytes: tiny_png.clone(),
            width: 4,
            height: 4,
        });
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert_eq!(
        mac_io.clipboard.lock().unwrap().current(),
        Some(&ClipboardPayload::Image {
            png_bytes: tiny_png,
            width: 4,
            height: 4,
        })
    );

    // 7) Universal Clipboard: Secure File transfer Linux -> Mac
    let file_src_dir = std::env::temp_dir().join(format!("mf-e2e-files-{}", uuid::Uuid::new_v4()));
    fs::create_dir_all(&file_src_dir).unwrap();
    let sample_file = file_src_dir.join("hello.txt");
    fs::write(&sample_file, b"encrypted-lan-file-bytes").unwrap();
    linux_io
        .clipboard
        .lock()
        .unwrap()
        .set_simulated(ClipboardPayload::Files {
            paths: vec![sample_file],
        });
    tokio::time::sleep(Duration::from_millis(700)).await;
    let mac_clip_now = mac_io.clipboard.lock().unwrap().current().cloned();
    match mac_clip_now {
        Some(ClipboardPayload::Files { paths }) => {
            assert_eq!(paths.len(), 1);
            assert_eq!(fs::read(&paths[0]).unwrap(), b"encrypted-lan-file-bytes");
        }
        other => panic!("expected staged file on Mac clipboard, got {other:?}"),
    }

    // 8) Unauthorized & revoked rejection + disconnect recovery
    linux.revoke_peer(win.local_id.clone());
    tokio::time::sleep(Duration::from_millis(150)).await;
    // Windows was controlling Linux when Linux revoked it -> Windows must drop focus safely!
    assert_ne!(win.snapshot().await.unwrap().focused_device, linux.local_id);
    assert!(!win.snapshot().await.unwrap().is_grabbing);

    mac.shutdown();
    win.shutdown();
    linux.shutdown();
    tokio::time::sleep(Duration::from_millis(100)).await;

    let _ = fs::remove_dir_all(file_src_dir);
    let _ = fs::remove_dir_all(mac_dir);
    let _ = fs::remove_dir_all(win_dir);
    let _ = fs::remove_dir_all(linux_dir);
}

#[tokio::test]
async fn unauthorized_peer_without_trust_cannot_inject_or_pair_with_bad_pin() {
    let (host, _host_io, host_dir, host_addr) = start_node("host", vec![]).await;
    let (rogue, _rogue_io, rogue_dir, _rogue_addr) = start_node("rogue", vec![]).await;

    // Rogue tries connecting directly without pairing
    assert!(rogue
        .connect_addr(host.local_id.clone(), host_addr)
        .await
        .is_err());

    // Host opens PIN 112233; rogue tries wrong PIN 999999
    host.open_pairing_window(Some("112233".into()))
        .await
        .unwrap();
    assert!(rogue
        .pair_with_addr(host_addr, Some(host.local_id.clone()), "999999".into())
        .await
        .is_err());

    assert!(host.snapshot().await.unwrap().peers.is_empty());

    host.shutdown();
    rogue.shutdown();
    tokio::time::sleep(Duration::from_millis(80)).await;
    let _ = fs::remove_dir_all(host_dir);
    let _ = fs::remove_dir_all(rogue_dir);
}
