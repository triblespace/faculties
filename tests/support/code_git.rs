//! Native, temporary-repository setup shared by Code consumer tests.

use std::path::Path;

pub fn initialize(dir: &Path) {
    let mut options = git2::RepositoryInitOptions::new();
    options.external_template(false).initial_head("main");
    let repo = git2::Repository::init_opts(dir, &options).unwrap();
    repo.config()
        .unwrap()
        .set_bool("core.autocrlf", false)
        .unwrap();
    let mut index = repo.index().unwrap();
    index
        .add_all(["."], git2::IndexAddOption::DEFAULT, None)
        .unwrap();
    index.write().unwrap();
    let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
    let signature = git2::Signature::new(
        "fixture",
        "fixture@example.invalid",
        &git2::Time::new(3600, 0),
    )
    .unwrap();
    repo.commit(Some("HEAD"), &signature, &signature, "fixture", &tree, &[])
        .unwrap();
}
