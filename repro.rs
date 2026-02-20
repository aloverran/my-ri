use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::fs;
use std::io::Write;

fn expand_includes(
    content: &str,
    base_dir: &Path,
    boundary: &Path,
    visited: &mut HashSet<PathBuf>,
) -> String {
    let mut result = String::with_capacity(content.len());
    let mut rest = content;

    // ISSUE HYPOTHESIS: strictly looks for "{{include:"
    while let Some(open) = rest.find("{{include:") {
        result.push_str(&rest[..open]);
        let after_open = &rest[open + "{{include:".len()..];
        match after_open.find("}}") {
            None => {
                result.push_str(&rest[open..]);
                rest = "";
                break;
            }
            Some(close) => {
                let raw_target = after_open[..close].trim();
                let replacement = resolve_include(raw_target, base_dir, boundary, visited);
                result.push_str(&replacement);
                rest = &after_open[close + "}}".len()..];
            }
        }
    }
    result.push_str(rest);
    result
}

fn resolve_include(
    target: &str,
    base_dir: &Path,
    boundary: &Path,
    visited: &mut HashSet<PathBuf>,
) -> String {
    if target.is_empty() {
        return "<!-- include error: empty target -->".to_string();
    }

    // Mocking file system behavior for test since we can't easily rely on real FS structure for edge cases in this snippet without setup
    // But actually, let's use the real logic and just create temp files.
    
    let resolved = base_dir.join(target);
    let canonical = match fs::canonicalize(&resolved) {
        Ok(c) => c,
        Err(_) => return format!("<!-- include error: not found {} -->", target),
    };

    if !canonical.starts_with(boundary) {
        return format!("<!-- include error: boundary -->");
    }

    if !visited.insert(canonical.clone()) {
        return format!("<!-- include error: cycle {} -->", target);
    }

    let raw = match fs::read_to_string(&canonical) {
        Ok(c) => c,
        Err(_) => {
             visited.remove(&canonical);
             return format!("<!-- include error: read -->");
        }
    };

    let include_dir = canonical.parent().unwrap().to_path_buf();
    let expanded = expand_includes(&raw, &include_dir, boundary, visited);

    visited.remove(&canonical);
    expanded
}

fn main() {
    // Setup test environment
    let root = std::env::current_dir().unwrap().join("test_env");
    if root.exists() { fs::remove_dir_all(&root).unwrap(); }
    fs::create_dir_all(&root).unwrap();

    let boundary = root.canonicalize().unwrap();
    let base = boundary.clone();

    // 1. Test Space Sensitivity
    let file1 = base.join("spaces.txt");
    fs::write(&file1, "Hello {{ include: other.txt }}").unwrap();
    let other = base.join("other.txt");
    fs::write(&other, "World").unwrap();

    let mut visited = HashSet::new();
    let res = expand_includes("Start {{ include: other.txt }} End", &base, &boundary, &mut visited);
    println!("Test 1 (Space handling):");
    println!("Input: 'Start {{{{ include: other.txt }}}} End'");
    println!("Output: '{}'", res);
    if res.contains("World") {
        println!("Result: PASSED (Unexpectedly?)");
    } else {
        println!("Result: FAILED (As expected)");
    }
    
    let res2 = expand_includes("Start {{include:other.txt}} End", &base, &boundary, &mut visited);
    println!("Test 1b (No Spaces):");
    println!("Output: '{}'", res2);

    // 2. Test Diamond Dependency
    // A -> B, A -> C, B -> D, C -> D
    let file_d = base.join("D.txt");
    fs::write(&file_d, "Leaf").unwrap();
    
    let file_b = base.join("B.txt");
    fs::write(&file_b, "B->{{include:D.txt}}").unwrap();
    
    let file_c = base.join("C.txt");
    fs::write(&file_c, "C->{{include:D.txt}}").unwrap();

    let file_a = base.join("A.txt");
    fs::write(&file_a, "Root: {{include:B.txt}} | {{include:C.txt}}").unwrap();

    let mut visited = HashSet::new();
    // We need to read A explicitly to start
    let content_a = fs::read_to_string(&file_a).unwrap();
    let res_diamond = expand_includes(&content_a, &base, &boundary, &mut visited);
    println!("\nTest 2 (Diamond):");
    println!("Output: '{}'", res_diamond);
    
    // 3. Test Cycle
    // X -> Y -> X
    let file_x = base.join("X.txt");
    fs::write(&file_x, "X calls {{include:Y.txt}}").unwrap();
    let file_y = base.join("Y.txt");
    fs::write(&file_y, "Y calls {{include:X.txt}}").unwrap();

    let mut visited = HashSet::new();
    let res_cycle = expand_includes(&fs::read_to_string(&file_x).unwrap(), &base, &boundary, &mut visited);
    println!("\nTest 3 (Cycle):");
    println!("Output: '{}'", res_cycle);

    // Cleanup
    fs::remove_dir_all(&root).unwrap();
}
