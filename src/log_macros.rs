macro_rules! log_info {
    ($($arg:tt)*) => {{
        let msg = std::ffi::CString::new(format!($($arg)*)).unwrap();
        crate::android_log_write(4, msg.as_ptr());
    }};
}

macro_rules! log_warn {
    ($($arg:tt)*) => {{
        let msg = std::ffi::CString::new(format!($($arg)*)).unwrap();
        crate::android_log_write(5, msg.as_ptr());
    }};
}

macro_rules! log_err {
    ($($arg:tt)*) => {{
        let msg = std::ffi::CString::new(format!($($arg)*)).unwrap();
        crate::android_log_write(6, msg.as_ptr());
    }};
}

macro_rules! log_debug {
    ($($arg:tt)*) => {{
        let msg = std::ffi::CString::new(format!($($arg)*)).unwrap();
        crate::android_log_write(3, msg.as_ptr());
    }};
}
