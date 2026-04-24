use super::*;

#[test]
fn git_workspace_status_uses_filtered_dirty_paths() {
    let repo = TestRepo::new();
    repo.init_git();
    let tracked = repo.write("tracked.rs", "const NAME: &str = \"BASE\";\n");
    repo.write(".gitignore", "ignored.rs\n");
    repo.commit_all("initial");
    repo.build();

    repo.write("tracked.rs", "const NAME: &str = \"DIRTY\";\n");
    let added = repo.write("added.rs", "const NAME: &str = \"ADDED\";\n");
    repo.write(".hidden.rs", "const NAME: &str = \"HIDDEN\";\n");
    repo.write("ignored.rs", "const NAME: &str = \"IGNORED\";\n");

    let workspace = WorkspaceIndex::open(WorkspaceIndexOptions {
        build_config: repo.build_config(),
    })
    .expect("workspace should open");
    let status = workspace.status().expect("status should succeed");
    let dirty = status.dirty_files.expect("dirty diff should exist");

    assert_eq!(
        dirty.modified_files,
        vec![tracked.to_string_lossy().into_owned()]
    );
    assert_eq!(dirty.deleted_files, Vec::<String>::new());
    assert_eq!(dirty.new_files, vec![added.to_string_lossy().into_owned()]);
}

#[test]
fn git_workspace_status_detects_files_revealed_by_gitignore_change() {
    let repo = TestRepo::new();
    repo.init_git();
    repo.write("tracked.rs", "const NAME: &str = \"BASE\";\n");
    repo.write(".gitignore", "revealed.rs\n");
    repo.commit_all("initial");
    repo.build();

    let revealed = repo.write("revealed.rs", "const NAME: &str = \"REVEALED\";\n");
    repo.write(".gitignore", "");

    let workspace = WorkspaceIndex::open(WorkspaceIndexOptions {
        build_config: repo.build_config(),
    })
    .expect("workspace should open");
    let status = workspace.status().expect("status should succeed");
    let dirty = status.dirty_files.expect("dirty diff should exist");

    assert_eq!(dirty.modified_files, Vec::<String>::new());
    assert_eq!(dirty.deleted_files, Vec::<String>::new());
    assert_eq!(
        dirty.new_files,
        vec![revealed.to_string_lossy().into_owned()]
    );
}

#[test]
fn git_workspace_status_skips_new_files_that_cannot_enter_the_index() {
    let repo = TestRepo::new();
    repo.init_git();
    repo.write("tracked.rs", "const NAME: &str = \"BASE\";\n");
    repo.commit_all("initial");
    repo.build();

    let mut bytes = vec![b'a'; 8 * 1024];
    bytes.push(0xFF);
    repo.write_bytes("encoded.txt", &bytes);

    let workspace = WorkspaceIndex::open(WorkspaceIndexOptions {
        build_config: repo.build_config(),
    })
    .expect("workspace should open");
    let status = workspace.status().expect("status should succeed");
    let dirty = status.dirty_files.expect("dirty diff should exist");

    assert_eq!(dirty.modified_files, Vec::<String>::new());
    assert_eq!(dirty.deleted_files, Vec::<String>::new());
    assert_eq!(dirty.new_files, Vec::<String>::new());
}
