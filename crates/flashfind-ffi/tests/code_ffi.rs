use flashfind::*;
use std::ffi::{CStr, CString, c_void};

unsafe extern "C" fn collect(ctx: *mut c_void, page: *const FfCodePage) {
    let out = unsafe { &mut *(ctx as *mut Vec<(String, String, u32)>) };
    let p = unsafe { &*page };
    for i in 0..p.count {
        let h = unsafe { &*p.hits.add(i) };
        let s = |c: *const std::ffi::c_char| unsafe { CStr::from_ptr(c) }.to_string_lossy().into_owned();
        out.push((s(h.repo), s(h.rel), h.group));
    }
}

#[test]
fn code_api_round_trip() {
    let home = std::env::temp_dir().join(format!("flashfind-ffi-code-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    for (rel, body) in [
        ("dev/one/.git/HEAD", "ref: refs/heads/main\n"),
        ("dev/one/src/router.rs", "x"),
        ("dev/one/node_modules/x/router.js", "x"),
        ("dev/two/.git/HEAD", "ref: refs/heads/main\n"),
        ("dev/two/app/router.py", "x"),
    ] {
        let p = home.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, body).unwrap();
    }
    let roots = CString::new(home.to_str().unwrap()).unwrap();
    let cache = CString::new(home.join(".cache").to_str().unwrap()).unwrap();
    unsafe {
        let mut err = std::ptr::null_mut();
        let c = ff_code_open(roots.as_ptr(), cache.as_ptr(), &mut err);
        assert!(!c.is_null());
        let mut st = FfCodeStats::default();
        ff_code_get_stats(c, &mut st);
        assert_eq!(st.repos, 2);
        let s = ff_code_session_new(c);
        let tok = ff_cancel_new();
        let mut out: Vec<(String, String, u32)> = Vec::new();
        let q = CString::new("router").unwrap();
        let rc = ff_code_search(s, q.as_ptr(), 10, tok, Some(collect), &mut out as *mut _ as *mut c_void, &mut err);
        assert_eq!(rc, FF_OK);
        let mut names: Vec<String> = out.iter().map(|(r, p, _)| format!("{r}:{p}")).collect();
        names.sort();
        assert_eq!(names, vec!["one:src/router.rs", "two:app/router.py"]);
        assert_eq!(out.iter().map(|x| x.2).max(), Some(1), "two groups");
        // Cancelled token: no callback.
        ff_cancel_cancel(tok);
        out.clear();
        let rc = ff_code_search(s, q.as_ptr(), 10, tok, Some(collect), &mut out as *mut _ as *mut c_void, &mut err);
        assert_eq!(rc, FF_CANCELLED);
        assert!(out.is_empty());
        // Bad repo: error string.
        let tok2 = ff_cancel_new();
        let q = CString::new("x repo:zzz").unwrap();
        let rc = ff_code_search(s, q.as_ptr(), 10, tok2, Some(collect), &mut out as *mut _ as *mut c_void, &mut err);
        assert_eq!(rc, FF_ERROR);
        assert!(CStr::from_ptr(err).to_string_lossy().contains("no project matches"));
        ff_string_free(err);
        let j = ff_code_projects_json(c, std::ptr::null(), 0);
        assert!(CStr::from_ptr(j).to_string_lossy().contains("\"total\":2"));
        ff_string_free(j);
        ff_cancel_free(tok);
        ff_cancel_free(tok2);
        ff_code_session_free(s);
        ff_code_free(c);
    }
    let _ = std::fs::remove_dir_all(home);
}
