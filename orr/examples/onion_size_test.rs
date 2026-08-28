//! Mide exactamente el tamaño del payload onion para body=64 B en cada modo.
use orr::onion::{build_onion, PathHopSecret};
use zeroize::Zeroizing;

fn main() {
    let body = vec![0xAAu8; 64];
    let ms = [0x42u8; 32];

    // Mode 1: path = [dst]
    let path1 = vec![PathHopSecret {
        orr_id: "orr_22".into(),
        master_secret: Zeroizing::new(ms),
        epoch_id: 0,
    }];
    let o1 = build_onion(&path1, body.clone(), 1, 1, b"h").unwrap();
    println!(
        "mode 1 (1 hop):   payload {} B  (body 64 + overhead {})",
        o1.payload.len(),
        o1.payload.len() as i32 - 64
    );

    // Mode 2: path = [mid, dst]
    let path2 = vec![
        PathHopSecret {
            orr_id: "orr_33".into(),
            master_secret: Zeroizing::new(ms),
            epoch_id: 0,
        },
        PathHopSecret {
            orr_id: "orr_22".into(),
            master_secret: Zeroizing::new(ms),
            epoch_id: 0,
        },
    ];
    let o2 = build_onion(&path2, body.clone(), 1, 1, b"h").unwrap();
    println!(
        "mode 2 (2 hops):  payload {} B  (body 64 + overhead {})",
        o2.payload.len(),
        o2.payload.len() as i32 - 64
    );

    // Mode -1: path = [mid1, mid2, dst]
    let path3 = vec![
        PathHopSecret {
            orr_id: "orr_33".into(),
            master_secret: Zeroizing::new(ms),
            epoch_id: 0,
        },
        PathHopSecret {
            orr_id: "orr_44".into(),
            master_secret: Zeroizing::new(ms),
            epoch_id: 0,
        },
        PathHopSecret {
            orr_id: "orr_22".into(),
            master_secret: Zeroizing::new(ms),
            epoch_id: 0,
        },
    ];
    let o3 = build_onion(&path3, body.clone(), 1, 1, b"h").unwrap();
    println!(
        "mode -1 (3 hops): payload {} B  (body 64 + overhead {})",
        o3.payload.len(),
        o3.payload.len() as i32 - 64
    );
}
