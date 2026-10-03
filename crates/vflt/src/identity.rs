//! Who is "me": USER, then USERNAME, then the whoami crate.

pub fn username() -> String {
    for var in ["USER", "USERNAME"] {
        if let Ok(v) = std::env::var(var) {
            let v = v.trim().to_string();
            if !v.is_empty() {
                return v;
            }
        }
    }
    let w = whoami::username();
    if w.trim().is_empty() {
        "human".to_string()
    } else {
        w
    }
}

pub fn hostname() -> String {
    let h = whoami::fallible::hostname().unwrap_or_default();
    if h.trim().is_empty() {
        vflt_core::file_store::hostname()
    } else {
        h
    }
}
