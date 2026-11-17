//! Exercise the actual C++ release scheduler embedded in the SurfaceFlinger patch.
use std::{fs, path::Path, process::Command};

#[test]
fn layer_pipeline_supports_bounded_async_imports_commits_and_legacy_replies() {
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let temp = tempfile::tempdir().unwrap();
    // Materialize the actual final transport after both patches, checking that
    // the modified new-file patch still composes with the release-fence patch.
    for patch in [
        "0007-droidloom-wayland-layers.patch",
        "0008-droidloom-release-fences.patch",
        "0011-droidloom-async-layer-commits.patch",
        "0012-droidloom-async-imports.patch",
        "0013-droidloom-lazy-render-targets.patch",
        "0014-droidloom-task-work-gate.patch",
        "0015-droidloom-scene-batches.patch",
        "0018-droidloom-import-retention.patch",
        "0025-droidloom-unplaceable-scenes-stay-composed.patch",
        "0026-droidloom-placement-grid.patch",
        "0027-droidloom-moving-scenes-stay-composed.patch",
    ] {
        let output = Command::new("git")
            .current_dir(temp.path())
            .args([
                "apply",
                "--include=services/surfaceflinger/DroidloomLayers.cpp",
                "--include=services/surfaceflinger/DroidloomLayers.h",
                "--include=services/surfaceflinger/DroidloomTaskWork.h",
                "--include=services/surfaceflinger/DroidloomImportRetention.h",
            ])
            .arg(repo.join("android/surfaceflinger").join(patch))
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let cpp = fs::read_to_string(
        temp.path()
            .join("services/surfaceflinger/DroidloomLayers.cpp"),
    )
    .unwrap();
    let start = cpp.find("uint32_t u32(").unwrap();
    let end = cpp.find("// Exact ABI of the pinned minigbm").unwrap();
    fs::write(
        temp.path().join("DroidloomLayerTransport.inc"),
        &cpp[start..end],
    )
    .unwrap();
    fs::copy(
        temp.path()
            .join("services/surfaceflinger/DroidloomTaskWork.h"),
        temp.path().join("DroidloomTaskWork.h"),
    )
    .unwrap();
    let binary = temp.path().join("layer-staging-test");
    let output = Command::new("c++")
        .args([
            "-std=c++20",
            "-Wall",
            "-Wextra",
            "-Werror",
            "-pthread",
            "-I",
        ])
        .arg(temp.path())
        .arg(repo.join("android/surfaceflinger/tests/layer_staging.cpp"))
        .arg("-o")
        .arg(&binary)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let status = Command::new("timeout")
        .arg("15s")
        .arg(binary)
        .status()
        .unwrap();
    assert!(
        status.success(),
        "layer pipeline failed or deadlocked: {status}"
    );
}

#[test]
fn host_fences_do_not_block_other_surfaces_or_commit_callbacks() {
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let temp = tempfile::tempdir().unwrap();
    // The scheduler as SurfaceFlinger builds it: 0008 adds it, 0021 changes it.
    for patch in [
        "0008-droidloom-release-fences.patch",
        "0021-droidloom-release-backlog-diagnostics.patch",
    ] {
        let output = Command::new("git")
            .current_dir(temp.path())
            .args([
                "apply",
                "--include=services/surfaceflinger/DroidloomReleaseQueue.h",
            ])
            .arg(repo.join("android/surfaceflinger").join(patch))
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    fs::copy(
        temp.path()
            .join("services/surfaceflinger/DroidloomReleaseQueue.h"),
        temp.path().join("DroidloomReleaseQueue.h"),
    )
    .unwrap();
    fs::create_dir(temp.path().join("log")).unwrap();
    fs::write(
        temp.path().join("log/log.h"),
        "#include <cstdio>\n#define ALOGW(...) std::fprintf(stderr, __VA_ARGS__)\n",
    )
    .unwrap();
    let binary = temp.path().join("release-queue-test");
    let output = Command::new("c++")
        .args([
            "-std=c++20",
            "-Wall",
            "-Wextra",
            "-Werror",
            "-pthread",
            "-I",
        ])
        .arg(temp.path())
        .arg(repo.join("android/surfaceflinger/tests/release_queue.cpp"))
        .arg("-o")
        .arg(&binary)
        .output()
        .expect("C++ compiler required for SurfaceFlinger regression test");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let status = Command::new("timeout")
        .arg("15s")
        .arg(binary)
        .status()
        .unwrap();
    assert!(
        status.success(),
        "release queue regression failed or deadlocked: {status}"
    );
}

#[test]
fn resize_imports_retire_after_release_without_churning_rotating_pools() {
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    // The policy as built: patch 0018 creates the header and later patches
    // amend it, so apply every patch touching it in series order.
    let temp = tempfile::tempdir().unwrap();
    let file = "services/surfaceflinger/DroidloomImportRetention.h";
    let mut patches: Vec<_> = fs::read_dir(repo.join("android/surfaceflinger"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "patch"))
        .collect();
    patches.sort();
    fs::create_dir_all(temp.path().join("services/surfaceflinger")).unwrap();
    let mut created = false;
    for path in patches {
        let text = fs::read_to_string(&path).unwrap();
        // This file's section: from its "--- " line (before "+++ ") to the
        // next file's.
        let lines: Vec<&str> = text.lines().collect();
        let Some(start) = lines
            .iter()
            .position(|line| line.starts_with("+++ ") && line.contains(file))
            .and_then(|plus| plus.checked_sub(1))
        else {
            continue;
        };
        let end = lines[start + 1..]
            .iter()
            .position(|line| line.starts_with("diff --git ") || line.starts_with("--- "))
            .map_or(lines.len(), |offset| start + 1 + offset);
        let section: String = lines[start..end]
            .iter()
            .map(|line| format!("{line}\n"))
            .collect();
        let section_path = temp.path().join("section.patch");
        fs::write(&section_path, &section).unwrap();
        let status = Command::new("patch")
            .args(["-p1", "-s", "--no-backup-if-mismatch", "-d"])
            .arg(temp.path())
            .arg("-i")
            .arg(&section_path)
            .status()
            .unwrap();
        assert!(status.success(), "{} does not apply", path.display());
        created = true;
    }
    assert!(created, "retention policy is part of the Android patches");
    fs::copy(
        temp.path().join(file),
        temp.path().join("DroidloomImportRetention.h"),
    )
    .unwrap();
    let binary = temp.path().join("import-retention-test");
    let output = Command::new("c++")
        .args(["-std=c++20", "-Wall", "-Wextra", "-Werror", "-I"])
        .arg(temp.path())
        .arg(repo.join("android/surfaceflinger/tests/import_retention.cpp"))
        .arg("-o")
        .arg(&binary)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let status = Command::new("timeout")
        .arg("15s")
        .arg(binary)
        .status()
        .unwrap();
    assert!(
        status.success(),
        "import retention regression failed: {status}"
    );
}
