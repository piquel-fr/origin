//! The HTTP API, driven through the SDK against a real server: every SDK
//! method, over the server's own repositories.

use std::collections::BTreeMap;
use std::slice;
use std::sync::Arc;
use std::time::{Duration, Instant};

use origin_client::{Client, Error, Repo};
use origin_shared::api::{
    BrowseQuery, ErrorCode, EventsQuery, Expected, FetchRecords, FetchSnapshot, InventoryRequest,
    LandRequest, List, LogQuery, NewTarget, PutQuery, RepoName, StatementsQuery, TokenRequest,
};
use origin_shared::event::EventKind;
use origin_shared::landing::{Blocked, Landing, Proposal};
use origin_shared::pack::Pack;
use origin_shared::{
    AttachmentId, Base, ChangeId, Digest, EncodedObject, EntryKind, EntryName, Inputs, Kind,
    NodeKey, Period, Record, RecordId, RepoPath, Revision, RevisionId, Source, Statement,
    TargetName, Tree, TreeEntry, TreeId, Verdict,
};
use origin_src::auth::{Credentials, StaticTokens};
use origin_src::http::Api;
use origin_src::policy::Policy;
use origin_src::server::Repositories;
use origin_src::service::{Node, OriginService};

/// A server hosting `demo`, whose policy requires nothing, and `strict`,
/// whose `main` requires a passing `ci.test` from `user:ci`, and where
/// only `user:ci` issues `ci.*` statements, `secrets` is withheld from
/// everyone but `user:ada`, and only she lands. The server's node,
/// `hosted`, orders both. `user:ada` and `user:ci` have static tokens.
struct Server {
    _dir: tempfile::TempDir,
    /// The server's base URL.
    base: String,
    client: Client,
    /// `demo` and `strict`, as `user:ada`.
    demo: Repo,
    strict: Repo,
    /// `strict`, as `user:ci`.
    ci: Repo,
    /// `strict`, anonymously.
    anonymous: Repo,
}

impl Server {
    const TOKENS: &str = "\"user:ada\" = \"ada-token\"\n\"user:ci\" = \"ci-token\"\n";

    async fn start() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let node = Node {
            name: "hosted".parse().unwrap(),
            key: Arc::new(NodeKey::from_bytes(&[7; 32])),
        };
        let open = Policy::initial(&node.name, &node.key.public());
        let strict = format!(
            "[nodes]\nhosted = \"{}\"\n[targets.policy]\nauthority = \"node:hosted\"\n\
             [targets.main]\nauthority = \"node:hosted\"\n\
             require = [{{ kind = \"ci.test\", verdict = \"pass\", issuer = \"user:ci\" }}]\n\
             [grants]\n\"user:ada\" = [\"land *\", \"annotate review\"]\n\
             \"user:ci\" = [\"annotate ci.*\"]\n\
             [[paths]]\npath = \"/\"\ngrants = {{ all = \"read\", \"user:ada\" = \"write\" }}\n\
             [[paths]]\npath = \"secrets\"\ngrants = {{ all = \"none\", \"user:ada\" = \"write\" }}\n",
            node.key.public()
        );
        let tokens: StaticTokens = toml::from_str(Self::TOKENS).unwrap();
        let mut repositories = Repositories::new(Credentials::new(node.clone(), Box::new(tokens)));
        for (name, policy) in [("demo", open), ("strict", strict)] {
            let service =
                OriginService::create(dir.path().join(name), node.clone(), &policy).unwrap();
            repositories.insert(name.parse().unwrap(), service);
        }

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, Api::router(repositories)).await });
        let anonymous = Client::new(&format!("http://{address}"));
        let client = anonymous.clone().with_token("ada-token");
        let ci = anonymous.clone().with_token("ci-token");
        Self {
            _dir: dir,
            base: format!("http://{address}"),
            demo: client.repo("demo".parse().unwrap()),
            strict: client.repo("strict".parse().unwrap()),
            ci: ci.repo("strict".parse().unwrap()),
            anonymous: anonymous.repo("strict".parse().unwrap()),
            client,
        }
    }
}

/// A revision of `change` with `files` and a description, made on `base`,
/// and every object it needs, records last.
struct Made {
    id: RevisionId,
    objects: Vec<EncodedObject>,
}

impl Made {
    fn revision(
        change: ChangeId,
        base: Base,
        predecessors: Vec<RevisionId>,
        files: &[(&str, &str)],
    ) -> Self {
        let mut objects: Vec<_> = files
            .iter()
            .map(|(_, content)| EncodedObject::blob(content.as_bytes()))
            .collect();
        let tree: Tree = files
            .iter()
            .zip(&objects)
            .map(|((name, _), blob)| {
                let entry = TreeEntry {
                    kind: EntryKind::File,
                    id: blob.id(),
                };
                (EntryName::new(*name).unwrap(), entry)
            })
            .collect();
        let tree = tree.encode();
        let description = EncodedObject::attachment(b"A change");
        let revision = Revision {
            change,
            predecessors,
            base,
            deps: Vec::new(),
            snapshot: tree.id().into(),
            description: AttachmentId::from(description.id()),
            author: "user:ada".parse().unwrap(),
        }
        .encode();
        let id = revision.id().into();
        objects.extend([tree, description, revision]);
        Self { id, objects }
    }

    /// A revision whose files may be in directories, such as
    /// `src/lib.rs`.
    fn nested(change: ChangeId, base: Base, files: &[(&str, &str)]) -> Self {
        let mut objects = Vec::new();
        let tree = Self::tree(files, &mut objects);
        let description = EncodedObject::attachment(b"A change");
        let revision = Revision {
            change,
            predecessors: Vec::new(),
            base,
            deps: Vec::new(),
            snapshot: tree,
            description: description.id().into(),
            author: "user:ada".parse().unwrap(),
        }
        .encode();
        let id = revision.id().into();
        objects.extend([description, revision]);
        Self { id, objects }
    }

    /// The tree of `files`, pushing its objects, children first.
    fn tree(files: &[(&str, &str)], objects: &mut Vec<EncodedObject>) -> TreeId {
        let mut dirs: BTreeMap<&str, Vec<(&str, &str)>> = BTreeMap::new();
        let mut tree = Tree::default();
        for &(path, content) in files {
            if let Some((dir, rest)) = path.split_once('/') {
                dirs.entry(dir).or_default().push((rest, content));
                continue;
            }
            let blob = EncodedObject::blob(content.as_bytes());
            let entry = TreeEntry {
                kind: EntryKind::File,
                id: blob.id(),
            };
            tree.insert(EntryName::new(path).unwrap(), entry);
            objects.push(blob);
        }
        for (dir, files) in dirs {
            let entry = TreeEntry {
                kind: EntryKind::Tree,
                id: Self::tree(&files, objects).into(),
            };
            tree.insert(EntryName::new(dir).unwrap(), entry);
        }
        let encoded = tree.encode();
        let id = encoded.id().into();
        objects.push(encoded);
        id
    }

    fn pack(&self) -> Pack {
        Pack {
            objects: self.objects.clone(),
        }
    }
}

/// The tip of `main`, to make revisions on.
async fn main_tip(repo: &Repo) -> Base {
    let targets = repo.targets().await.unwrap();
    let main = targets
        .iter()
        .find(|target| target.name == TargetName::MAIN)
        .unwrap();
    Base::Acceptance(main.head.acceptance)
}

fn ci_test(subject: RevisionId, issuer: &str) -> EncodedObject {
    Statement {
        subject: RecordId::Revision(subject),
        kind: Kind::new("ci.test").unwrap(),
        verdict: Some(Verdict::Pass),
        issuer: issuer.parse().unwrap(),
        inputs: None,
        payload: None,
        supersedes: None,
        promoted_from: None,
    }
    .encode()
}

fn change(seed: u8) -> ChangeId {
    ChangeId::from_bytes([seed; 16])
}

/// The error code of a refused request.
fn code(error: Error) -> ErrorCode {
    match error {
        Error::Api(error) => error.code,
        error => panic!("expected an API error, got {error}"),
    }
}

#[tokio::test]
async fn hosts_several_repositories() {
    let server = Server::start().await;
    let names: Vec<_> = server.client.repos().await.unwrap();
    let names: Vec<_> = names.iter().map(|repo| repo.name.as_str()).collect();
    assert_eq!(names, ["demo", "strict"]);

    let missing = server.client.repo(RepoName::new("missing").unwrap());
    assert_eq!(
        code(missing.targets().await.unwrap_err()),
        ErrorCode::NotFound
    );

    // Records put in one repository aren't in the other.
    let made = Made::revision(change(1), main_tip(&server.demo).await, vec![], &[]);
    server.demo.put_records(&made.pack()).await.unwrap();
    let request = FetchRecords {
        ids: vec![made.id.into()],
    };
    assert!(server.demo.fetch_records(&request).await.is_ok());
    let error = server.strict.fetch_records(&request).await.unwrap_err();
    assert_eq!(code(error), ErrorCode::NotFound);
}

#[tokio::test]
async fn puts_and_fetches_records() {
    let server = Server::start().await;
    let repo = &server.demo;
    let made = Made::revision(
        change(1),
        main_tip(repo).await,
        vec![],
        &[("a.txt", "a"), ("b.txt", "b")],
    );
    let stored = repo.put_records(&made.pack()).await.unwrap();
    assert_eq!(
        stored.new, 5,
        "two blobs, the tree, the description and the revision"
    );
    let again = repo.put_records(&made.pack()).await.unwrap();
    assert_eq!(
        again.new, 0,
        "records are immutable, so putting them again does nothing"
    );

    let [a, _, tree, description, revision] = &made.objects[..] else {
        unreachable!()
    };
    let fetched = repo
        .fetch_records(&FetchRecords {
            ids: vec![revision.id()],
        })
        .await
        .unwrap();
    assert_eq!(
        fetched.pack.objects,
        [revision.clone(), description.clone()],
        "verified, with the description"
    );
    for content in [a, tree] {
        let error = repo
            .fetch_records(&FetchRecords {
                ids: vec![content.id()],
            })
            .await
            .unwrap_err();
        assert_eq!(code(error), ErrorCode::BadRequest, "content leaves by path");
    }

    // A record is refused unless the repository can use it.
    let orphan = Made::revision(change(2), main_tip(repo).await, vec![], &[("c.txt", "c")]);
    let revision_only = Pack {
        objects: vec![orphan.objects.last().unwrap().clone()],
    };
    let error = repo.put_records(&revision_only).await.unwrap_err();
    assert_eq!(code(error), ErrorCode::BadRequest);
    let log = repo
        .log(&LogQuery {
            target: TargetName::MAIN,
            since: 0,
            limit: None,
        })
        .await
        .unwrap();
    let acceptance = log[0].acceptance.encode();
    let error = repo
        .put_records(&Pack {
            objects: vec![acceptance],
        })
        .await
        .unwrap_err();
    assert_eq!(
        code(error),
        ErrorCode::BadRequest,
        "only the authority lands"
    );
}

#[tokio::test]
async fn puts_refuse_objects_named_as_another_kind() {
    let server = Server::start().await;
    let repo = &server.demo;
    let base = main_tip(repo).await;
    let refused = |objects: Vec<EncodedObject>| async move {
        code(repo.put_records(&Pack { objects }).await.unwrap_err())
    };

    // A revision whose snapshot is a blob, in the batch.
    let blob = EncodedObject::blob(b"not a tree");
    let description = EncodedObject::attachment(b"A change");
    let revision = Revision {
        change: change(1),
        predecessors: Vec::new(),
        base,
        deps: Vec::new(),
        snapshot: blob.id().into(),
        description: description.id().into(),
        author: "user:ada".parse().unwrap(),
    }
    .encode();
    let objects = vec![blob.clone(), description, revision];
    assert_eq!(refused(objects).await, ErrorCode::BadRequest);

    // Trees naming a stored blob as a directory, and a stored tree as a
    // file.
    let made = Made::nested(change(2), base, &[("src/lib.rs", "fn a() {}")]);
    repo.put_records(&made.pack()).await.unwrap();
    let src = made.objects[1].id();
    for (kind, id) in [
        (EntryKind::Tree, made.objects[0].id()),
        (EntryKind::File, src),
    ] {
        let tree: Tree = [(EntryName::new("x").unwrap(), TreeEntry { kind, id })]
            .into_iter()
            .collect();
        assert_eq!(refused(vec![tree.encode()]).await, ErrorCode::BadRequest);
    }

    // One stored object needed as two kinds, the right one last: a
    // revision's snapshot and description, and a tree's directory and
    // file.
    let attachment = EncodedObject::attachment(b"stored");
    let blob = made.objects[0].clone();
    repo.put_records(&Pack {
        objects: vec![attachment.clone()],
    })
    .await
    .unwrap();
    let both = Revision {
        change: change(3),
        predecessors: Vec::new(),
        base,
        deps: Vec::new(),
        snapshot: attachment.id().into(),
        description: attachment.id().into(),
        author: "user:ada".parse().unwrap(),
    };
    assert_eq!(refused(vec![both.encode()]).await, ErrorCode::BadRequest);
    let entry = |kind| TreeEntry {
        kind,
        id: blob.id(),
    };
    let tree: Tree = [
        (EntryName::new("a").unwrap(), entry(EntryKind::Tree)),
        (EntryName::new("b").unwrap(), entry(EntryKind::File)),
    ]
    .into_iter()
    .collect();
    assert_eq!(refused(vec![tree.encode()]).await, ErrorCode::BadRequest);
}

#[tokio::test]
async fn pushes_compare_and_swap_the_heads_they_saw() {
    let server = Server::start().await;
    let repo = &server.demo;
    let base = main_tip(repo).await;
    let expect = |heads: &[RevisionId]| PutQuery {
        expect: List(vec![Expected {
            change: change(1),
            heads: heads.to_vec(),
        }]),
    };
    let first = Made::revision(change(1), base, Vec::new(), &[("a", "first")]);
    repo.push_records(&first.pack(), &expect(&[]))
        .await
        .unwrap();

    // Two pushes made from the same view of the change: the second is
    // refused, and publishes no record.
    let left = Made::revision(change(1), base, vec![first.id], &[("a", "left")]);
    let right = Made::revision(change(1), base, vec![first.id], &[("a", "right")]);
    repo.push_records(&left.pack(), &expect(&[first.id]))
        .await
        .unwrap();
    let error = repo
        .push_records(&right.pack(), &expect(&[first.id]))
        .await
        .unwrap_err();
    assert_eq!(code(error), ErrorCode::Conflict);
    let request = InventoryRequest {
        changes: vec![change(1)],
    };
    assert_eq!(repo.inventory(&request).await.unwrap()[0].heads, [left.id]);
    repo.push_records(&right.pack(), &expect(&[left.id]))
        .await
        .unwrap();
}

#[tokio::test]
async fn promotions_must_match_their_original() {
    let server = Server::start().await;
    let repo = &server.demo;
    let made = Made::revision(change(1), main_tip(repo).await, vec![], &[("a.txt", "a")]);
    repo.put_records(&made.pack()).await.unwrap();
    // Even when they come together.
    let same = Made::revision(change(3), main_tip(repo).await, vec![], &[("a.txt", "a")]);
    repo.put_records(&same.pack()).await.unwrap();
    let original = Statement {
        subject: RecordId::Revision(made.id),
        kind: Kind::new("ci.test").unwrap(),
        verdict: Some(Verdict::Pass),
        issuer: "user:ada".parse().unwrap(),
        inputs: Some(Inputs {
            paths: vec!["a.txt".parse().unwrap()],
            definition: Digest::of(b"test"),
            toolchain: Digest::of(b"rustc"),
            external: false,
        }),
        payload: None,
        supersedes: None,
        promoted_from: None,
    };
    let promotion = Statement {
        subject: RecordId::Revision(same.id),
        promoted_from: Some(original.encode().id().into()),
        ..original.clone()
    };
    let forged = Statement {
        verdict: Some(Verdict::Fail),
        ..promotion.clone()
    };
    let error = repo
        .put_records(&Pack {
            objects: vec![original.encode(), forged.encode()],
        })
        .await
        .unwrap_err();
    assert_eq!(code(error), ErrorCode::BadRequest);
    let stored = repo
        .put_records(&Pack {
            objects: vec![promotion.encode(), original.encode()],
        })
        .await
        .unwrap();
    assert_eq!(stored.new, 1, "the original was stored with the forgery");
}

#[tokio::test]
async fn fetches_snapshots_by_walking_them() {
    let server = Server::start().await;
    let repo = &server.demo;
    let made = Made::revision(
        change(1),
        main_tip(repo).await,
        vec![],
        &[("a.txt", "a"), ("b.txt", "b")],
    );
    repo.put_records(&made.pack()).await.unwrap();
    let [a, b, tree, description, _] = &made.objects[..] else {
        unreachable!()
    };

    let snapshot = |have: Vec<TreeId>, paths: &[&str]| FetchSnapshot {
        of: RecordId::Revision(made.id),
        have,
        paths: paths
            .iter()
            .map(|path| RepoPath::new(path).unwrap())
            .collect(),
        skip: 0,
    };
    let whole = repo.fetch_snapshot(&snapshot(vec![], &[])).await.unwrap();
    assert_eq!(whole.pack.objects, [tree.clone(), a.clone(), b.clone()]);
    let only_b = repo
        .fetch_snapshot(&snapshot(vec![], &["b.txt"]))
        .await
        .unwrap();
    assert_eq!(only_b.pack.objects, [tree.clone(), b.clone()]);
    let edited = Made::revision(
        change(3),
        main_tip(repo).await,
        vec![],
        &[("a.txt", "a"), ("b.txt", "c")],
    );
    repo.put_records(&edited.pack()).await.unwrap();
    let changed = repo
        .fetch_snapshot(&FetchSnapshot {
            of: RecordId::Revision(edited.id),
            have: vec![tree.id().into()],
            paths: Vec::new(),
            skip: 0,
        })
        .await
        .unwrap();
    assert_eq!(
        changed.pack.objects,
        [edited.objects[2].clone(), edited.objects[1].clone()],
        "the new tree and the edited file"
    );
    let subtree = FetchSnapshot {
        of: RecordId::Snapshot(tree.id().into()),
        have: Vec::new(),
        paths: Vec::new(),
        skip: 0,
    };
    let error = repo.fetch_snapshot(&subtree).await.unwrap_err();
    assert_eq!(
        code(error),
        ErrorCode::BadRequest,
        "only a record's snapshot, not a tree within one"
    );
    let missing = FetchSnapshot {
        of: RecordId::Revision(description.id().into()),
        ..subtree
    };
    let error = repo.fetch_snapshot(&missing).await.unwrap_err();
    assert_eq!(code(error), ErrorCode::NotFound);
}

#[tokio::test]
async fn reads_statements_and_revisions() {
    let server = Server::start().await;
    let repo = &server.demo;
    let base = main_tip(repo).await;
    let first = Made::revision(change(1), base, vec![], &[("a.txt", "1")]);
    let statement = ci_test(first.id, "user:ada");
    let mut pack = first.pack();
    pack.objects.push(statement.clone());
    repo.put_records(&pack).await.unwrap();

    let about_first = StatementsQuery {
        subject: RecordId::Revision(first.id),
    };
    let found = repo.statements(&about_first).await.unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].id, statement.id().into());
    assert_eq!(found[0].statement.kind.as_str(), "ci.test");
    assert_eq!(found[0].superseded_by, None);

    // Two revisions evolving from the same one make the change diverge,
    // which the server flags.
    let left = Made::revision(change(1), base, vec![first.id], &[("a.txt", "2")]);
    let right = Made::revision(change(1), base, vec![first.id], &[("a.txt", "3")]);
    for made in [&left, &right] {
        repo.put_records(&made.pack()).await.unwrap();
    }
    let revisions = repo.revisions(change(1)).await.unwrap();
    let ids: Vec<_> = revisions.revisions.iter().map(|entry| entry.id).collect();
    assert_eq!(ids, [first.id, left.id, right.id]);
    assert_eq!(revisions.heads, [left.id, right.id]);
    let mut flagged = Vec::new();
    for head in [left.id, right.id] {
        let query = StatementsQuery {
            subject: RecordId::Revision(head),
        };
        flagged.extend(repo.statements(&query).await.unwrap());
    }
    assert_eq!(flagged.len(), 1, "one statement names both heads");
    assert_eq!(flagged[0].statement.kind, Kind::DIVERGENT);
    assert_eq!(flagged[0].statement.issuer.to_string(), "node:hosted");

    let error = repo.revisions(change(9)).await.unwrap_err();
    assert_eq!(code(error), ErrorCode::NotFound);

    let inventories = repo
        .inventory(&InventoryRequest {
            changes: vec![change(1), change(9)],
        })
        .await
        .unwrap();
    let [known, unknown] = &inventories[..] else {
        panic!("{inventories:?}")
    };
    assert_eq!(known.revisions, ids);
    assert_eq!(known.heads, [left.id, right.id]);
    let mut statements = known.statements.clone();
    statements.sort();
    let mut expected = vec![statement.id().into(), flagged[0].id];
    expected.sort();
    assert_eq!(statements, expected);
    assert!(unknown.revisions.is_empty() && unknown.statements.is_empty());
}

#[tokio::test]
async fn lands_changes_and_targets() {
    let server = Server::start().await;
    let repo = &server.demo;
    let made = Made::revision(change(1), main_tip(repo).await, vec![], &[("a.txt", "a")]);
    repo.put_records(&made.pack()).await.unwrap();

    let land = LandRequest {
        proposal: Proposal::Change(change(1)),
        onto: None,
    };
    let landed = repo.land(&land).await.unwrap();
    assert_eq!(landed.onto, TargetName::MAIN, "the target the change is on");
    let [
        Landing::Landed {
            source, acceptance, ..
        },
    ] = landed.landings[..]
    else {
        panic!("expected a landing, got {:?}", landed.landings);
    };
    assert_eq!(
        source,
        Source::Revision(made.id),
        "made on the tip, so not rebased"
    );
    assert_eq!(acceptance.seq, 1);

    let log = repo
        .log(&LogQuery {
            target: TargetName::MAIN,
            since: 1,
            limit: Some(1),
        })
        .await
        .unwrap();
    assert_eq!(log.len(), 1);
    assert_eq!(log[0].id, acceptance.acceptance);
    assert_eq!(log[0].acceptance.body().landed, [Source::Revision(made.id)]);

    let release = TargetName::new("release").unwrap();
    let new = NewTarget {
        name: release.clone(),
        from: TargetName::MAIN,
    };
    assert_eq!(repo.create_target(&new).await.unwrap().seq, 0);
    let error = repo.create_target(&new).await.unwrap_err();
    assert_eq!(code(error), ErrorCode::Conflict, "the name is taken");
    let targets = repo.targets().await.unwrap();
    let names: Vec<_> = targets.iter().map(|target| target.name.as_str()).collect();
    assert_eq!(names, ["main", "policy", "release"]);
    assert!(
        targets
            .iter()
            .all(|target| target.authority == Some("hosted".parse().unwrap()))
    );

    let from_main = LandRequest {
        proposal: Proposal::Target(TargetName::MAIN),
        onto: Some(release),
    };
    let landed = repo.land(&from_main).await.unwrap();
    assert!(matches!(landed.landings[..], [Landing::Unchanged(_)]));
    let nowhere = LandRequest {
        onto: None,
        ..from_main
    };
    assert_eq!(
        code(repo.land(&nowhere).await.unwrap_err()),
        ErrorCode::BadRequest
    );
    let unknown = LandRequest {
        proposal: Proposal::Change(change(9)),
        onto: None,
    };
    assert_eq!(
        code(repo.land(&unknown).await.unwrap_err()),
        ErrorCode::NotFound
    );
}

#[tokio::test]
async fn landings_wait_for_evidence() {
    let server = Server::start().await;
    let repo = &server.strict;
    let made = Made::revision(change(1), main_tip(repo).await, vec![], &[("a.txt", "a")]);
    repo.put_records(&made.pack()).await.unwrap();
    let land = LandRequest {
        proposal: Proposal::Change(change(1)),
        onto: None,
    };

    let landed = repo.land(&land).await.unwrap();
    let [Landing::Pending(pending)] = &landed.landings[..] else {
        panic!("expected a waiting landing, got {:?}", landed.landings);
    };
    assert_eq!(pending.subject, RecordId::Revision(made.id));
    assert!(matches!(&pending.blocked, Blocked::Evidence(missing) if missing.len() == 1));
    assert_eq!(repo.landings().await.unwrap(), slice::from_ref(pending));

    let query = EventsQuery {
        cursor: Some(0),
        kind: List(vec!["check.requested".parse().unwrap()]),
        ..EventsQuery::default()
    };
    let page = repo.events(&query).await.unwrap();
    assert!(
        matches!(&page.events[..], [event] if matches!(event.kind,
            EventKind::CheckRequested { subject, .. } if subject == RecordId::Revision(made.id))),
        "{page:?}"
    );

    // Only `user:ci` issues them, and only as itself.
    let forged = Pack {
        objects: vec![ci_test(made.id, "user:ci")],
    };
    assert_eq!(
        code(repo.put_records(&forged).await.unwrap_err()),
        ErrorCode::Forbidden
    );
    let own = Pack {
        objects: vec![ci_test(made.id, "user:ada")],
    };
    assert_eq!(
        code(repo.put_records(&own).await.unwrap_err()),
        ErrorCode::Forbidden
    );
    server.ci.put_records(&forged).await.unwrap();
    let landed = repo.land(&land).await.unwrap();
    assert!(matches!(landed.landings[..], [Landing::Landed { .. }]));
    assert_eq!(repo.landings().await.unwrap(), []);
}

#[tokio::test]
async fn events_long_poll() {
    let server = Server::start().await;
    let repo = server.demo.clone();
    let now = repo.events(&EventsQuery::default()).await.unwrap();
    assert!(
        now.events.is_empty(),
        "without a cursor, only new events come"
    );

    let query = EventsQuery {
        cursor: Some(now.next),
        wait: Some(Period(Duration::from_secs(30))),
        kind: List(vec!["revision.created".parse().unwrap()]),
        ..EventsQuery::default()
    };
    let start = Instant::now();
    let poll = tokio::spawn(async move { repo.events(&query).await.unwrap() });
    tokio::time::sleep(Duration::from_millis(100)).await;
    let made = Made::revision(change(1), main_tip(&server.demo).await, vec![], &[]);
    server.demo.put_records(&made.pack()).await.unwrap();
    let page = poll.await.unwrap();
    assert!(
        start.elapsed() < Duration::from_secs(10),
        "it answered once a revision came"
    );
    assert!(
        matches!(&page.events[..], [event] if event.kind.record() == RecordId::Revision(made.id)),
        "{page:?}"
    );
    assert!(page.next > now.next);

    let all = server
        .demo
        .events(&EventsQuery {
            cursor: Some(0),
            limit: Some(1),
            ..EventsQuery::default()
        })
        .await
        .unwrap();
    assert!(
        matches!(&all.events[..], [event] if event.kind.name() == "target.advanced"),
        "the first event is `policy`'s first acceptance: {all:?}"
    );
}

/// Lands a change of `user:ada` with `files` on `strict`'s `main`, with the
/// `ci.test` it requires.
async fn land_on_strict(server: &Server, seed: u8, files: &[(&str, &str)]) -> Made {
    let made = Made::nested(change(seed), main_tip(&server.strict).await, files);
    server.strict.put_records(&made.pack()).await.unwrap();
    server
        .ci
        .put_records(&Pack {
            objects: vec![ci_test(made.id, "user:ci")],
        })
        .await
        .unwrap();
    let land = LandRequest {
        proposal: Proposal::Change(change(seed)),
        onto: None,
    };
    let landed = server.strict.land(&land).await.unwrap();
    assert!(
        matches!(landed.landings[..], [Landing::Landed { .. }]),
        "{landed:?}"
    );
    made
}

#[tokio::test]
async fn tells_callers_what_they_may_do_and_issues_narrower_tokens() {
    let server = Server::start().await;
    let path = |path: &str| RepoPath::new(path).unwrap();
    let ada = server.strict.whoami().await.unwrap();
    assert_eq!(ada.principal, Some("user:ada".parse().unwrap()));
    assert!(ada.paths.writes(&path("secrets/key")) && ada.may_land(&TargetName::MAIN));
    let anonymous = server.anonymous.whoami().await.unwrap();
    assert_eq!(anonymous.principal, None);
    assert!(anonymous.paths.reads(&path("README.md")));
    assert!(!anonymous.paths.reads(&path("secrets/key")));
    let stranger = Client::new(&server.base).with_token("nope");
    assert_eq!(
        code(stranger.repos().await.unwrap_err()),
        ErrorCode::Unauthorized
    );

    // Ada hands an agent a token that only writes `src`.
    let scope = |scope: &[&str]| TokenRequest {
        scope: scope
            .iter()
            .map(|capability| capability.parse().unwrap())
            .collect(),
        ttl: Period(Duration::from_hours(1)),
    };
    let issued = server
        .strict
        .issue_token(&scope(&["write src"]))
        .await
        .unwrap();
    assert_eq!(issued.principal, "user:ada".parse().unwrap());
    let agent = Client::new(&server.base)
        .with_token(issued.token)
        .repo("strict".parse().unwrap());
    let rights = agent.whoami().await.unwrap();
    assert!(rights.paths.writes(&path("src/lib.rs")));
    assert!(!rights.paths.reads(&path("README.md")));
    assert!(rights.land.is_empty() && !rights.admin);
    // Its token only narrows: what it lacks, it cannot pass on.
    for wider in [&["write /"][..], &["land main"], &["read secrets"]] {
        let error = agent.issue_token(&scope(wider)).await.unwrap_err();
        assert_eq!(code(error), ErrorCode::Forbidden, "{wider:?}");
    }
    assert!(agent.issue_token(&scope(&["write src/net"])).await.is_ok());
    let error = server
        .anonymous
        .issue_token(&scope(&["read /"]))
        .await
        .unwrap_err();
    assert_eq!(code(error), ErrorCode::Forbidden);

    // The agent proposes changes to `src`, and nothing else, and lands
    // nothing.
    let base = main_tip(&server.strict).await;
    let in_src = Made::nested(change(1), base, &[("src/lib.rs", "fn a() {}")]);
    agent.put_records(&in_src.pack()).await.unwrap();
    let outside = Made::nested(change(2), base, &[("README.md", "hi")]);
    let error = agent.put_records(&outside.pack()).await.unwrap_err();
    assert_eq!(code(error), ErrorCode::Forbidden);
    let land = LandRequest {
        proposal: Proposal::Change(change(1)),
        onto: None,
    };
    assert_eq!(
        code(agent.land(&land).await.unwrap_err()),
        ErrorCode::Forbidden
    );
    // Anonymous callers only read.
    let error = server
        .anonymous
        .put_records(&in_src.pack())
        .await
        .unwrap_err();
    assert_eq!(code(error), ErrorCode::Forbidden);
}

#[tokio::test]
async fn browses_snapshots_without_their_history() {
    let server = Server::start().await;
    let made = land_on_strict(
        &server,
        1,
        &[
            ("README.md", "hello"),
            ("secrets/key", "hunter2"),
            ("src/lib.rs", "fn a() {}"),
        ],
    )
    .await;
    let at = |at: &str, path: &str| BrowseQuery {
        at: at.parse().unwrap(),
        path: RepoPath::new(path).unwrap(),
    };
    let root = server.strict.tree(&at("main", "/")).await.unwrap();
    let names: Vec<_> = root
        .entries
        .iter()
        .map(|entry| (entry.name.as_str(), entry.kind, entry.withheld))
        .collect();
    assert_eq!(
        names,
        [
            ("README.md", EntryKind::File, false),
            ("secrets", EntryKind::Tree, false),
            ("src", EntryKind::Tree, false),
        ]
    );
    let revision = format!("revision:{}", made.id);
    assert_eq!(
        server
            .strict
            .file(&at(&revision, "secrets/key"))
            .await
            .unwrap(),
        b"hunter2"
    );

    // Others see that the secret is there, but not what it is.
    let anonymous = &server.anonymous;
    let root = anonymous.tree(&at("main", "")).await.unwrap();
    assert!(root.entries[1].withheld, "{root:?}");
    assert_eq!(
        anonymous.file(&at("main", "README.md")).await.unwrap(),
        b"hello"
    );
    for path in ["secrets/key", "secrets"] {
        let error = anonymous.file(&at("main", path)).await.unwrap_err();
        assert_eq!(code(error), ErrorCode::Forbidden, "{path}");
    }
    let error = anonymous.tree(&at("main", "secrets")).await.unwrap_err();
    assert_eq!(code(error), ErrorCode::Forbidden);
    let error = anonymous.tree(&at("main", "README.md")).await.unwrap_err();
    assert_eq!(code(error), ErrorCode::BadRequest);
    let error = anonymous.file(&at("main", "missing")).await.unwrap_err();
    assert_eq!(code(error), ErrorCode::NotFound);
    let error = anonymous.tree(&at("nowhere", "")).await.unwrap_err();
    assert_eq!(code(error), ErrorCode::NotFound);
}

#[tokio::test]
async fn reads_only_what_the_caller_may() {
    let server = Server::start().await;
    let public = land_on_strict(
        &server,
        1,
        &[("README.md", "hello"), ("secrets/key", "hunter2")],
    )
    .await;
    let secret = Made::nested(
        change(2),
        main_tip(&server.strict).await,
        &[("README.md", "hello"), ("secrets/key", "rotated")],
    );
    server.strict.put_records(&secret.pack()).await.unwrap();
    let anonymous = &server.anonymous;

    // The walk withholds the secret, but keeps its place in the tree.
    let walked = anonymous
        .fetch_snapshot(&FetchSnapshot {
            of: RecordId::Revision(public.id),
            have: Vec::new(),
            paths: Vec::new(),
            skip: 0,
        })
        .await
        .unwrap();
    let ids: Vec<_> = walked.pack.objects.iter().map(EncodedObject::id).collect();
    let hunter2 = EncodedObject::blob(b"hunter2").id();
    assert!(!ids.contains(&hunter2), "the secret's content is withheld");
    assert_eq!(ids.len(), 2, "the root and README.md, not `secrets`'s tree");
    let root: Tree = walked.pack.objects[0].decode_as().unwrap();
    assert!(root.get("secrets").is_some());

    // A revision that only changes what they can't read is hidden, with
    // everything about it.
    let records = anonymous
        .fetch_records(&FetchRecords {
            ids: vec![public.id.into(), secret.id.into()],
        })
        .await
        .unwrap();
    assert_eq!(records.pack.objects[0].id(), public.id.into());
    assert!(
        records
            .pack
            .objects
            .iter()
            .all(|object| object.id() != secret.id.into())
    );
    let error = anonymous.revisions(change(2)).await.unwrap_err();
    assert_eq!(code(error), ErrorCode::NotFound);
    let query = StatementsQuery {
        subject: RecordId::Revision(secret.id),
    };
    server
        .strict
        .put_records(&Pack {
            objects: vec![review(secret.id)],
        })
        .await
        .unwrap();
    assert_eq!(server.strict.statements(&query).await.unwrap().len(), 1);
    assert_eq!(anonymous.statements(&query).await.unwrap(), []);
    let error = anonymous
        .fetch_snapshot(&FetchSnapshot {
            of: RecordId::Revision(secret.id),
            have: Vec::new(),
            paths: Vec::new(),
            skip: 0,
        })
        .await
        .unwrap_err();
    assert_eq!(code(error), ErrorCode::NotFound);
    let everything = EventsQuery {
        cursor: Some(0),
        ..EventsQuery::default()
    };
    let hidden = |page: &origin_shared::event::Page| {
        page.events.iter().any(|event| {
            matches!(event.kind,
                EventKind::RevisionCreated { revision, .. } if revision == secret.id)
                || matches!(event.kind,
                    EventKind::StatementIssued { subject, .. } if subject == RecordId::Revision(secret.id))
        })
    };
    assert!(hidden(&server.strict.events(&everything).await.unwrap()));
    assert!(!hidden(&anonymous.events(&everything).await.unwrap()));
}

/// A passing review from `user:ada` about `subject`.
fn review(subject: RevisionId) -> EncodedObject {
    Statement {
        kind: Kind::new("review").unwrap(),
        issuer: "user:ada".parse().unwrap(),
        ..ci_test(subject, "user:ada").decode_as().unwrap()
    }
    .encode()
}

#[tokio::test]
async fn serves_its_openapi_document() {
    let server = Server::start().await;
    let document = server.client.openapi().await.unwrap();
    assert_eq!(document, serde_json::to_value(Api::openapi()).unwrap());
}

/// `docs/openapi.json` documents the API for other languages. Run `just
/// openapi` to rewrite it after changing the API.
#[test]
fn checked_in_openapi_document_is_current() {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../docs/openapi.json");
    let document = format!("{}\n", Api::openapi().to_pretty_json().unwrap());
    if std::env::var_os("UPDATE_OPENAPI").is_some() {
        std::fs::write(path, document).unwrap();
    } else {
        let checked_in = std::fs::read_to_string(path).unwrap();
        assert!(
            checked_in == document,
            "docs/openapi.json is stale: run `just openapi`"
        );
    }
}
