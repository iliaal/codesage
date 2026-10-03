use std::path::Path;
use std::process::Command;

use serde_json::{Value, json};

fn write(root: &Path, path: &str, source: &str) {
    let path = root.join(path);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, source).unwrap();
}

fn run(root: &Path, args: &[&str]) -> String {
    let output = Command::new(env!("CARGO_BIN_EXE_codesage"))
        .current_dir(root)
        .env("CODESAGE_WATCH", "0")
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{args:?}: {}\n{}",
        String::from_utf8_lossy(&output.stderr),
        String::from_utf8_lossy(&output.stdout)
    );
    String::from_utf8(output.stdout).unwrap()
}

fn query(root: &Path, args: &[&str]) -> Value {
    serde_json::from_str(&run(root, args)).unwrap()
}

#[test]
fn annotated_and_commented_type_names_preserve_roles_without_promoting_arguments() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    maven_fixture(root);
    write(
        root,
        "src/main/java/probe/A.java",
        "package probe; @java.lang.annotation.Target(java.lang.annotation.ElementType.TYPE_USE) public @interface A { String value() default \"\"; }",
    );
    let positives = [
        ("Plain", "class Plain { java.net.http.HttpClient field; }"),
        (
            "Qualified",
            "class Qualified { java.net.http.@A HttpClient field; }",
        ),
        (
            "Simple",
            "import java.net.http.HttpClient; class Simple { @A HttpClient field; }",
        ),
        (
            "Array",
            "class Array { java.net.http.@A HttpClient @A [] field; }",
        ),
        (
            "Owner",
            "class Owner { java.net.http.HttpClient.@A Builder field; }",
        ),
        (
            "QualifiedComment",
            "class QualifiedComment { java.net./* comment */http.HttpClient field; }",
        ),
        (
            "ImportComment",
            "import java.net./* comment */http.HttpClient; class ImportComment { HttpClient field; }",
        ),
        (
            "WildcardComment",
            "import java.net./* comment */http.*; class WildcardComment { @A HttpClient[] field; }",
        ),
        (
            "StaticComment",
            "import static java.net.http./* comment */HttpClient.Builder; interface StaticComment { @A Builder[] FIELD = null; }",
        ),
        (
            "OwnerImport",
            "import java.net./* comment */http.HttpClient; class OwnerImport { HttpClient.@A Builder[] field; }",
        ),
        (
            "Arguments",
            "class Arguments { java.net.http.@A(\"java.util.List\") HttpClient field; }",
        ),
        (
            "AnnotationName",
            "class AnnotationName { java.net.http.@probe.A HttpClient field; }",
        ),
        (
            "LineComment",
            "class LineComment { java.net.// comment\nhttp.HttpClient field; }",
        ),
    ];
    for (name, declaration) in positives {
        write(
            root,
            &format!("src/main/java/probe/{name}.java"),
            &format!("package probe; {declaration}"),
        );
    }
    for (name, declaration) in [
        (
            "Container",
            "class Container { java.util.@A List<java.net.http.@A HttpClient>[] field; }",
        ),
        (
            "ContainerBuilder",
            "class ContainerBuilder { java.util.List<java.net.http.HttpClient.@A Builder> field; }",
        ),
        (
            "Lookalike",
            "class Lookalike { class HttpClient {} @A HttpClient[] field; }",
        ),
        (
            "Generic",
            "class Generic<HttpClient> { @A HttpClient[] field; }",
        ),
    ] {
        write(
            root,
            &format!("src/main/java/probe/{name}.java"),
            &format!("package probe; {declaration}"),
        );
    }
    write(
        root,
        "src/main/java/shadow/java.java",
        "package shadow; public class java { public static class net { public static class http { public static class HttpClient {} } } }",
    );
    write(
        root,
        "src/main/java/shadow/Head.java",
        "package shadow; class Head { java.net.http.@probe.A HttpClient field; }",
    );
    write(
        root,
        "src/main/java/probe/ImportedHead.java",
        "package probe; import shadow./* comment */java; class ImportedHead { java.net.http.@A HttpClient field; }",
    );
    run(root, &["index", "--no-semantic"]);
    for (name, _) in positives {
        assert_eq!(
            query(
                root,
                &[
                    "trust-boundaries",
                    &format!("src/main/java/probe/{name}.java"),
                    "--json"
                ]
            )["trust_boundaries"],
            json!(["network", "external-api", "serialization"]),
            "{name}"
        );
    }
    for path in [
        "src/main/java/probe/Container.java",
        "src/main/java/probe/ContainerBuilder.java",
        "src/main/java/probe/Lookalike.java",
        "src/main/java/probe/Generic.java",
        "src/main/java/shadow/Head.java",
        "src/main/java/probe/ImportedHead.java",
    ] {
        assert_eq!(
            query(root, &["trust-boundaries", path, "--json"])["trust_boundaries"],
            json!([]),
            "{path}"
        );
    }
    let features = query(
        root,
        &["features-list", "--tag", "external-client", "--json"],
    );
    assert_eq!(features["results"].as_array().unwrap().len(), 1);
    let members: Vec<_> = features["results"][0]["files"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|file| file["role"] != "context")
        .map(|file| file["path"].as_str().unwrap())
        .collect();
    let mut expected: Vec<_> = positives
        .iter()
        .map(|(name, _)| format!("src/main/java/probe/{name}.java"))
        .collect();
    expected.sort();
    assert_eq!(members, expected);
    let plain = query(root, &["risk", "src/main/java/probe/Plain.java", "--json"]);
    let qualified = query(
        root,
        &["risk", "src/main/java/probe/Qualified.java", "--json"],
    );
    assert_eq!(qualified["score"], plain["score"]);
    assert!(qualified["notes"].as_array().unwrap().iter().any(|note| {
        note.as_str()
            .unwrap()
            .contains("crosses 3 trust boundaries")
    }));
}

#[test]
fn maven_test_sources_see_main_declarations_without_reversing_or_crossing_modules() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    maven_fixture(root);
    write(root, "child/pom.xml", "<project/>");
    let main_head = "package example; public class java { public static class net { public static class http { public static class HttpClient {} } } }";
    let main_owner = "package example; public class Heads { public static class java { public static class net { public static class http { public static class HttpClient {} } } } }";
    write(root, "src/main/java/example/java.java", main_head);
    write(root, "src/main/java/example/Heads.java", main_owner);
    for (path, source) in [
        (
            "src/test/java/example/Same.java",
            "package example; class Same { java.net.http.HttpClient field; }",
        ),
        (
            "src/test/java/other/Imported.java",
            "package other; import example.*; class Imported { java.net.http.HttpClient field; }",
        ),
        (
            "src/test/java/other/Static.java",
            "package other; import static example.Heads.*; class Static { java.net.http.HttpClient field; }",
        ),
        (
            "src/main/java/reverse/Main.java",
            "package reverse; class Main { java.net.http.HttpClient field; }",
        ),
        (
            "src/test/java/reverse/java.java",
            "package reverse; public class java { public static class net { public static class http { public static class HttpClient {} } } }",
        ),
        (
            "child/src/main/java/example/Child.java",
            "package example; class Child { java.net.http.HttpClient field; }",
        ),
    ] {
        write(root, path, source);
    }
    let test_paths = [
        "src/test/java/example/Same.java",
        "src/test/java/other/Imported.java",
        "src/test/java/other/Static.java",
    ];
    run(root, &["index", "--no-semantic"]);
    for path in test_paths {
        assert_eq!(
            query(root, &["trust-boundaries", path, "--json"])["trust_boundaries"],
            json!([]),
            "{path}"
        );
    }
    for path in [
        "src/main/java/reverse/Main.java",
        "child/src/main/java/example/Child.java",
    ] {
        assert_eq!(
            query(root, &["trust-boundaries", path, "--json"])["trust_boundaries"],
            json!(["network", "external-api", "serialization"]),
            "{path}"
        );
    }
    let features = query(
        root,
        &["features-list", "--tag", "external-client", "--json"],
    );
    assert_eq!(features["results"].as_array().unwrap().len(), 2);
    for feature in features["results"].as_array().unwrap() {
        assert!(feature["files"].as_array().unwrap().iter().all(|file| {
            file["role"] == "context" || !file["path"].as_str().unwrap().contains("/test/")
        }));
    }
    std::fs::remove_file(root.join("src/main/java/example/java.java")).unwrap();
    write(
        root,
        "src/main/java/example/Heads.java",
        "package example; public class Heads { public static Object java; }",
    );
    run(root, &["index", "--no-semantic", "--no-features"]);
    for path in test_paths {
        assert_eq!(
            query(root, &["trust-boundaries", path, "--json"])["trust_boundaries"],
            json!(["network", "external-api", "serialization"]),
            "removed main type: {path}"
        );
    }
    write(root, "src/main/java/example/java.java", main_head);
    write(root, "src/main/java/example/Heads.java", main_owner);
    run(root, &["index", "--no-semantic", "--no-features"]);
    for path in test_paths {
        assert_eq!(
            query(root, &["trust-boundaries", path, "--json"])["trust_boundaries"],
            json!([]),
            "restored main type: {path}"
        );
    }
}

#[test]
fn wildcard_type_candidates_respect_declaration_accessibility() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    maven_fixture(root);
    write(
        root,
        "src/main/java/example/Heads.java",
        "package example; public class Heads { private static class java { public static class net { public static class http { public static class HttpClient {} } } } }",
    );
    write(
        root,
        "src/main/java/example/java.java",
        "package example; class java { public static class net { public static class http { public static class HttpClient {} } } }",
    );
    write(
        root,
        "src/main/java/protectedcase/Heads.java",
        "package protectedcase; public class Heads { protected static class java { public static class net { public static class http { public static class HttpClient {} } } } }",
    );
    for (name, imports) in [
        ("Static", "import static example.Heads.*;"),
        ("Ordinary", "import example.Heads.*;"),
        ("Package", "import example.*;"),
        ("Protected", "import static protectedcase.Heads.*;"),
    ] {
        write(
            root,
            &format!("src/main/java/other/{name}.java"),
            &format!("package other; {imports} class {name} {{ java.net.http.HttpClient field; }}"),
        );
    }
    write(
        root,
        "src/main/java/example/Same.java",
        "package example; class Same { java.net.http.HttpClient field; }",
    );
    write(
        root,
        "src/main/java/protectedcase/Same.java",
        "package protectedcase; import static protectedcase.Heads.*; class Same { java.net.http.HttpClient field; }",
    );
    run(root, &["index", "--no-semantic"]);
    for name in ["Static", "Ordinary", "Package", "Protected"] {
        assert_eq!(
            query(
                root,
                &[
                    "trust-boundaries",
                    &format!("src/main/java/other/{name}.java"),
                    "--json"
                ]
            )["trust_boundaries"],
            json!(["network", "external-api", "serialization"]),
            "{name}"
        );
    }
    for package in ["example", "protectedcase"] {
        assert_eq!(
            query(
                root,
                &[
                    "trust-boundaries",
                    &format!("src/main/java/{package}/Same.java"),
                    "--json"
                ]
            )["trust_boundaries"],
            json!([]),
            "{package}"
        );
    }
    let features = query(
        root,
        &["features-list", "--tag", "external-client", "--json"],
    );
    let members: Vec<_> = features["results"][0]["files"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|file| file["role"] != "context")
        .map(|file| file["path"].as_str().unwrap())
        .collect();
    assert_eq!(
        members,
        [
            "src/main/java/other/Ordinary.java",
            "src/main/java/other/Package.java",
            "src/main/java/other/Protected.java",
            "src/main/java/other/Static.java"
        ]
    );
}

#[test]
fn on_demand_member_imports_resolve_types_separately_from_values() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    maven_fixture(root);
    let heads = "package example; public class Heads { public static class java { public static class net { public static class http { public static class HttpClient {} } } } public static class org { public static class springframework { public static class web { public static class bind { public static class annotation { public @interface RestController {} } } } } } public static class Builder {} }";
    write(root, "src/main/java/example/Heads.java", heads);
    write(
        root,
        "src/main/java/example/Values.java",
        "package example; public class Values { public static Object java; public static void org() {} }",
    );
    write(
        root,
        "src/main/java/example/InnerHeads.java",
        "package example; public class InnerHeads { public class java { public class net { public class http { public class HttpClient {} } } } }",
    );
    let negatives = [
        (
            "StaticClient",
            "package bad; import static example.Heads.*; class StaticClient { java.net.http.HttpClient field; }",
        ),
        (
            "StaticWeb",
            "package bad; import static example.Heads.*; @org.springframework.web.bind.annotation.RestController class StaticWeb {}",
        ),
        (
            "OrdinaryClient",
            "package bad; import example.Heads.*; class OrdinaryClient { java.net.http.HttpClient field; }",
        ),
        (
            "OrdinaryWeb",
            "package bad; import example.Heads.*; @org.springframework.web.bind.annotation.RestController class OrdinaryWeb {}",
        ),
        (
            "OrdinaryInner",
            "package bad; import example.InnerHeads.*; class OrdinaryInner { java.net.http.HttpClient field; }",
        ),
    ];
    for (name, source) in negatives {
        write(root, &format!("src/main/java/bad/{name}.java"), source);
    }
    let positives = [
        (
            "ValueClient",
            "package good; import static example.Values.*; class ValueClient { java.net.http.HttpClient field; }",
        ),
        (
            "StaticInner",
            "package good; import static example.InnerHeads.*; class StaticInner { java.net.http.HttpClient field; }",
        ),
        (
            "Explicit",
            "package good; import java.net.http.HttpClient.Builder; import static example.Heads.*; class Explicit { Builder field; }",
        ),
    ];
    for (name, source) in positives {
        write(root, &format!("src/main/java/good/{name}.java"), source);
    }
    run(root, &["index", "--no-semantic"]);
    for (name, _) in negatives {
        assert_eq!(
            query(
                root,
                &[
                    "trust-boundaries",
                    &format!("src/main/java/bad/{name}.java"),
                    "--json"
                ]
            )["trust_boundaries"],
            json!([]),
            "{name}"
        );
    }
    for (name, _) in positives {
        assert_eq!(
            query(
                root,
                &[
                    "trust-boundaries",
                    &format!("src/main/java/good/{name}.java"),
                    "--json"
                ]
            )["trust_boundaries"],
            json!(["network", "external-api", "serialization"]),
            "{name}"
        );
    }
    assert!(
        query(
            root,
            &["features-list", "--tag", "web-entrypoint", "--json"]
        )["results"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let features = query(
        root,
        &["features-list", "--tag", "external-client", "--json"],
    );
    let members: Vec<_> = features["results"][0]["files"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|file| file["role"] != "context")
        .map(|file| file["path"].as_str().unwrap())
        .collect();
    assert_eq!(
        members,
        [
            "src/main/java/good/Explicit.java",
            "src/main/java/good/StaticInner.java",
            "src/main/java/good/ValueClient.java"
        ]
    );
    let user = "src/main/java/bad/StaticClient.java";
    let unchanged = std::fs::read(root.join(user)).unwrap();
    write(
        root,
        "src/main/java/example/Heads.java",
        "package example; public class Heads { public static Object java; public static void org() {} public static class Builder {} }",
    );
    run(root, &["index", "--no-semantic", "--no-features"]);
    assert_eq!(std::fs::read(root.join(user)).unwrap(), unchanged);
    assert_eq!(
        query(root, &["trust-boundaries", user, "--json"])["trust_boundaries"],
        json!(["network", "external-api", "serialization"])
    );
    run(root, &["map", "--json"]);
    write(root, "src/main/java/example/Heads.java", heads);
    run(root, &["index", "--no-semantic"]);
    assert_eq!(
        query(root, &["trust-boundaries", user, "--json"])["trust_boundaries"],
        json!([])
    );
}

#[test]
fn interface_and_annotation_client_fields_match_class_field_roles() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    maven_fixture(root);
    let positives = [
        (
            "Qualified",
            "interface Qualified { java.net.http.HttpClient FIELD = null; }",
        ),
        (
            "Imported",
            "import java.net.http.HttpClient; interface Imported { HttpClient FIELD = null; }",
        ),
        (
            "Array",
            "interface Array { java.net.http.HttpClient[][] FIELD = null; }",
        ),
        (
            "Suffix",
            "interface Suffix { java.net.http.HttpClient FIELD[][] = null; }",
        ),
        (
            "Static",
            "import static java.net.http.HttpClient.Builder; interface Static { Builder[] FIELD = null; }",
        ),
        (
            "Annotation",
            "@interface Annotation { java.net.http.HttpClient FIELD = null; }",
        ),
        ("Class", "class Class { java.net.http.HttpClient FIELD; }"),
    ];
    let negatives = [
        (
            "Container",
            "interface Container { java.util.List<java.net.http.HttpClient>[] FIELD = null; }",
            json!([]),
        ),
        (
            "ImportedContainer",
            "import java.net.http.HttpClient; interface ImportedContainer { java.util.List<HttpClient> FIELD = null; }",
            json!(["network", "external-api"]),
        ),
        (
            "Lookalike",
            "interface Lookalike { class HttpClient {} HttpClient[] FIELD = null; }",
            json!([]),
        ),
        (
            "Plain",
            "interface Plain { String FIELD = null; }",
            json!([]),
        ),
    ];
    for (name, source) in positives {
        write(root, &format!("src/main/java/{name}.java"), source);
    }
    for (name, source, _) in &negatives {
        write(root, &format!("src/main/java/{name}.java"), source);
    }
    run(root, &["index", "--no-semantic"]);
    for (name, _) in positives {
        assert_eq!(
            query(
                root,
                &[
                    "trust-boundaries",
                    &format!("src/main/java/{name}.java"),
                    "--json"
                ]
            )["trust_boundaries"],
            json!(["network", "external-api", "serialization"]),
            "{name}"
        );
    }
    for (name, _, tags) in negatives {
        assert_eq!(
            query(
                root,
                &[
                    "trust-boundaries",
                    &format!("src/main/java/{name}.java"),
                    "--json"
                ]
            )["trust_boundaries"],
            tags,
            "{name}"
        );
    }
    let features = query(
        root,
        &["features-list", "--tag", "external-client", "--json"],
    );
    let members: Vec<_> = features["results"][0]["files"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|file| file["role"] != "context")
        .map(|file| file["path"].as_str().unwrap())
        .collect();
    let mut expected: Vec<_> = positives
        .iter()
        .map(|(name, _)| format!("src/main/java/{name}.java"))
        .collect();
    expected.sort();
    assert_eq!(members, expected);
    let risk = query(root, &["risk", "src/main/java/Qualified.java", "--json"]);
    assert_eq!(
        risk["trust_boundaries"],
        json!(["network", "external-api", "serialization"])
    );
    assert!(risk["notes"].as_array().unwrap().iter().any(|note| {
        note.as_str()
            .unwrap()
            .contains("crosses 3 trust boundaries")
    }));
}

#[test]
fn qualified_framework_spellings_respect_type_heads_in_every_namespace() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    maven_fixture(root);
    for (head, members) in [
        (
            "org",
            "public static class springframework { public static class web { public static class bind { public static class annotation { public @interface RestController {} } } } }",
        ),
        (
            "java",
            "public static class net { public static class http { public static class HttpClient {} } }",
        ),
        (
            "javax",
            "public static class persistence { public @interface Entity {} }",
        ),
        (
            "jakarta",
            "public static class ws { public static class rs { public @interface Path { String value(); } } }",
        ),
        ("okhttp3", "public static class OkHttpClient {}"),
    ] {
        write(
            root,
            &format!("src/main/java/example/{head}.java"),
            &format!("package example; public class {head} {{ {members} }}"),
        );
    }
    let mut negatives = Vec::new();
    for (name, head, declaration) in [
        (
            "Web",
            "org",
            "@org.springframework.web.bind.annotation.RestController class NAME {}",
        ),
        (
            "Client",
            "java",
            "class NAME { java.net.http.HttpClient field; }",
        ),
        ("Entity", "javax", "@javax.persistence.Entity class NAME {}"),
        (
            "Path",
            "jakarta",
            "@jakarta.ws.rs.Path(\"/orders\") class NAME {}",
        ),
        (
            "OkHttp",
            "okhttp3",
            "class NAME { okhttp3.OkHttpClient field; }",
        ),
    ] {
        for (prefix, package, imports) in [
            ("Same", "example", String::new()),
            ("Imported", "other", format!("import example.{head};")),
            ("Wildcard", "wild", "import example.*;".to_string()),
        ] {
            let name = format!("{prefix}{name}");
            let path = format!("src/main/java/{package}/{name}.java");
            write(
                root,
                &path,
                &format!(
                    "package {package}; {imports} {}",
                    declaration.replace("NAME", &name)
                ),
            );
            negatives.push(path);
        }
    }
    for (name, source) in [
        (
            "Real",
            "package positive; class Real { java.net.http.HttpClient field; }",
        ),
        (
            "Owner",
            "package positive; import java.net.http.HttpClient; class Owner { HttpClient.Builder field; }",
        ),
        (
            "Web",
            "package positive; @org.springframework.web.bind.annotation.RestController class Web {}",
        ),
    ] {
        write(root, &format!("src/main/java/positive/{name}.java"), source);
    }
    run(root, &["index", "--no-semantic"]);
    for path in &negatives {
        assert_eq!(
            query(root, &["trust-boundaries", path, "--json"])["trust_boundaries"],
            json!([]),
            "{path}"
        );
    }
    let features = query(root, &["features-list", "--lang", "java", "--json"]);
    assert_eq!(features["results"].as_array().unwrap().len(), 2);
    for feature in features["results"].as_array().unwrap() {
        for file in feature["files"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|file| file["role"] != "context")
        {
            assert!(
                file["path"]
                    .as_str()
                    .unwrap()
                    .starts_with("src/main/java/positive/"),
                "{file}"
            );
        }
    }
    for name in ["Real", "Owner"] {
        assert_eq!(
            query(
                root,
                &[
                    "trust-boundaries",
                    &format!("src/main/java/positive/{name}.java"),
                    "--json"
                ]
            )["trust_boundaries"],
            json!(["network", "external-api", "serialization"])
        );
    }
    let unchanged = std::fs::read(root.join("src/main/java/example/SameClient.java")).unwrap();
    std::fs::remove_file(root.join("src/main/java/example/java.java")).unwrap();
    run(root, &["index", "--no-semantic", "--no-features"]);
    assert_eq!(
        std::fs::read(root.join("src/main/java/example/SameClient.java")).unwrap(),
        unchanged
    );
    assert_eq!(
        query(
            root,
            &[
                "trust-boundaries",
                "src/main/java/example/SameClient.java",
                "--json"
            ]
        )["trust_boundaries"],
        json!(["network", "external-api", "serialization"])
    );
    run(root, &["map", "--json"]);
    write(
        root,
        "src/main/java/example/java.java",
        "package example; public class java { public static class net { public static class http { public static class HttpClient {} } } }",
    );
    run(root, &["index", "--no-semantic"]);
    assert_eq!(
        query(
            root,
            &[
                "trust-boundaries",
                "src/main/java/example/SameClient.java",
                "--json"
            ]
        )["trust_boundaries"],
        json!([])
    );
}

#[test]
fn client_array_spellings_have_equal_roles_without_promoting_containers_or_lookalikes() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    maven_fixture(root);
    let positives = [
        (
            "Qualified",
            "class Qualified { java.net.http.HttpClient[] field; }",
        ),
        (
            "QualifiedSuffix",
            "class QualifiedSuffix { java.net.http.HttpClient field[]; }",
        ),
        (
            "QualifiedMulti",
            "class QualifiedMulti { java.net.http.HttpClient[][] field; }",
        ),
        (
            "QualifiedMixed",
            "class QualifiedMixed { java.net.http.HttpClient[] field[]; }",
        ),
        (
            "Simple",
            "import java.net.http.HttpClient; class Simple { HttpClient[] field; }",
        ),
        (
            "SimpleSuffix",
            "import java.net.http.HttpClient; class SimpleSuffix { HttpClient field[][]; }",
        ),
        (
            "Owner",
            "import java.net.http.HttpClient; class Owner { HttpClient.Builder[] field; }",
        ),
        (
            "Static",
            "import static java.net.http.HttpClient.Builder; class Static { Builder[][] field; }",
        ),
    ];
    let negatives = [
        (
            "Container",
            "class Container { java.util.List<java.net.http.HttpClient>[] field; }",
            json!([]),
        ),
        (
            "Generic",
            "import java.net.http.HttpClient; class Generic<HttpClient> { HttpClient[] field; }",
            json!(["network", "external-api"]),
        ),
        (
            "Local",
            "class Local { class HttpClient {} HttpClient[][] field; }",
            json!([]),
        ),
        (
            "Unrelated",
            "class Unrelated { example.HttpClient[] field; }",
            json!([]),
        ),
    ];
    for (name, source) in positives {
        write(root, &format!("src/main/java/{name}.java"), source);
    }
    for (name, source, _) in &negatives {
        write(root, &format!("src/main/java/{name}.java"), source);
    }
    run(root, &["index", "--no-semantic"]);
    for (name, _) in positives {
        assert_eq!(
            query(
                root,
                &[
                    "trust-boundaries",
                    &format!("src/main/java/{name}.java"),
                    "--json"
                ]
            )["trust_boundaries"],
            json!(["network", "external-api", "serialization"]),
            "{name}"
        );
    }
    for (name, _, expected) in negatives {
        assert_eq!(
            query(
                root,
                &[
                    "trust-boundaries",
                    &format!("src/main/java/{name}.java"),
                    "--json"
                ]
            )["trust_boundaries"],
            expected,
            "{name}"
        );
    }
    let features = query(
        root,
        &["features-list", "--tag", "external-client", "--json"],
    );
    let members: Vec<_> = features["results"][0]["files"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|file| file["role"] != "context")
        .map(|file| file["path"].as_str().unwrap())
        .collect();
    let mut expected: Vec<_> = positives
        .iter()
        .map(|(name, _)| format!("src/main/java/{name}.java"))
        .collect();
    expected.sort();
    assert_eq!(members, expected);
}

#[test]
fn unavailable_java_context_preserves_file_feature_and_risk_facts_until_retry() {
    for no_features in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        maven_fixture(root);
        let healthy = "src/main/java/example/Healthy.java";
        let broken = "src/main/java/example/Broken.java";
        write(
            root,
            healthy,
            "package example; import org.springframework.web.bind.annotation.*; @RestController class Healthy {}",
        );
        write(root, broken, "package example; class Broken {} /* valid */");
        run(root, &["index", "--no-semantic"]);
        let before_tags =
            query(root, &["trust-boundaries", healthy, "--json"])["trust_boundaries"].clone();
        let before_features =
            query(root, &["features-list", "--lang", "java", "--json"])["results"].clone();
        let before_risk = query(root, &["risk", healthy, "--json"]);
        std::fs::write(
            root.join(broken),
            b"package example; class Broken {} /*\xff*/",
        )
        .unwrap();
        let mut args = vec!["index", "--no-semantic"];
        if no_features {
            args.push("--no-features");
        }
        for _ in 0..2 {
            let output = Command::new(env!("CARGO_BIN_EXE_codesage"))
                .current_dir(root)
                .env("CODESAGE_WATCH", "0")
                .args(&args)
                .output()
                .unwrap();
            assert!(
                !output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stdout)
            );
            assert!(String::from_utf8_lossy(&output.stderr).contains("Java declaration context"));
            assert!(String::from_utf8_lossy(&output.stderr).contains("structural: 2 failed files"));
            assert_eq!(
                query(root, &["trust-boundaries", healthy, "--json"])["trust_boundaries"],
                before_tags
            );
            assert_eq!(
                query(root, &["features-list", "--lang", "java", "--json"])["results"],
                before_features
            );
            let risk = query(root, &["risk", healthy, "--json"]);
            assert_eq!(risk["score"], before_risk["score"]);
            assert_eq!(risk["trust_boundaries"], before_risk["trust_boundaries"]);
            assert!(risk["notes"].as_array().unwrap().iter().any(|note| {
                note.as_str()
                    .unwrap()
                    .contains("crosses 3 trust boundaries")
            }));
            assert_eq!(
                query(root, &["status", "--json"])["interpretation"]["stale_files"],
                2
            );
        }
        write(
            root,
            broken,
            "package example; class Broken {} /* repaired */",
        );
        run(root, &args);
        assert_eq!(
            query(root, &["trust-boundaries", healthy, "--json"])["trust_boundaries"],
            before_tags
        );
        assert_eq!(
            query(root, &["status", "--json"])["interpretation"]["stale_files"],
            0
        );
        run(root, &args);
    }
}

#[test]
fn single_static_member_types_override_client_wildcards_but_values_do_not() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    maven_fixture(root);
    write(
        root,
        "src/main/java/example/Holder.java",
        "package example; public class Holder { public static class Builder {} public static class Nested { public static class Builder {} } }",
    );
    write(
        root,
        "src/main/java/example/Implicit.java",
        "package example; public interface Implicit { class Builder {} }",
    );
    write(
        root,
        "src/main/java/example/Enums.java",
        "package example; public class Enums { public enum Builder { ONE } }",
    );
    write(
        root,
        "src/main/java/example/Methods.java",
        "package example; public class Methods { public static void Builder() {} }",
    );
    write(
        root,
        "src/main/java/example/Fields.java",
        "package example; public class Fields { public static Object Builder; }",
    );
    write(
        root,
        "src/main/java/example/Constants.java",
        "package example; public interface Constants { Object Builder = null; }",
    );
    write(
        root,
        "src/main/java/example/EnumValues.java",
        "package example; public enum EnumValues { Builder }",
    );
    for (name, imported) in [
        ("Type", "example.Holder.Builder"),
        ("Nested", "example.Holder.Nested.Builder"),
        ("Implicit", "example.Implicit.Builder"),
        ("Enum", "example.Enums.Builder"),
        ("Method", "example.Methods.Builder"),
        ("Field", "example.Fields.Builder"),
        ("Constant", "example.Constants.Builder"),
        ("EnumValue", "example.EnumValues.Builder"),
        ("Unknown", "external.Holder.Builder"),
    ] {
        write(
            root,
            &format!("src/main/java/{name}.java"),
            &format!(
                "import static {imported}; import static java.net.http.HttpClient.*; class {name} {{ Builder field; }}"
            ),
        );
    }
    run(root, &["index", "--no-semantic"]);
    write(
        root,
        "src/main/java/Ordinary.java",
        "import example.Holder.Builder; import static java.net.http.HttpClient.*; class Ordinary { Builder field; }",
    );
    run(root, &["map", "--json"]);
    run(root, &["index", "--no-semantic"]);
    for name in ["Type", "Nested", "Implicit", "Enum", "Ordinary", "Unknown"] {
        assert_eq!(
            query(
                root,
                &[
                    "trust-boundaries",
                    &format!("src/main/java/{name}.java"),
                    "--json"
                ]
            )["trust_boundaries"],
            json!(["network", "external-api"])
        );
    }
    for name in ["Method", "Field", "Constant", "EnumValue"] {
        assert_eq!(
            query(
                root,
                &[
                    "trust-boundaries",
                    &format!("src/main/java/{name}.java"),
                    "--json"
                ]
            )["trust_boundaries"],
            json!(["network", "external-api", "serialization"])
        );
    }
    let features = query(
        root,
        &["features-list", "--tag", "external-client", "--json"],
    );
    assert_eq!(features["results"].as_array().unwrap().len(), 1);
    let members = features["results"][0]["files"].as_array().unwrap();
    assert!(
        members
            .iter()
            .filter(|file| file["role"] != "context")
            .all(|file| [
                "src/main/java/Method.java",
                "src/main/java/Field.java",
                "src/main/java/Constant.java",
                "src/main/java/EnumValue.java"
            ]
            .iter()
            .any(|path| file["path"] == *path))
    );
    assert!(
        members
            .iter()
            .any(|file| file["path"] == "src/main/java/Method.java")
    );
    assert!(
        members
            .iter()
            .any(|file| file["path"] == "src/main/java/Field.java")
    );
}

fn maven_fixture(root: &Path) {
    write(
        root,
        ".codesage/config.toml",
        "[project]\nname = \"java-review\"\n[embedding]\nmodel = \"jinaai/jina-embeddings-v2-base-code\"\ndevice = \"cpu\"\n",
    );
    write(root, "pom.xml", "<project/>");
}

#[test]
fn java_package_declarations_refresh_unchanged_wildcard_users() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    maven_fixture(root);
    let user = "src/main/java/example/Plain.java";
    let sibling = "src/main/java/example/RestController.java";
    write(
        root,
        user,
        "package example; import org.springframework.web.bind.annotation.*; @RestController class Plain {}",
    );
    write(
        root,
        "src/main/java/other/Other.java",
        "package other; import org.springframework.web.bind.annotation.*; @RestController class Other {}",
    );
    write(
        root,
        "src/main/java/example/Explicit.java",
        "package example; import org.springframework.web.bind.annotation.RestController; @RestController class Explicit {}",
    );
    write(root, "child/pom.xml", "<project/>");
    write(
        root,
        "child/src/main/java/example/Child.java",
        "package example; import org.springframework.web.bind.annotation.*; @RestController class Child {}",
    );
    run(root, &["index", "--no-semantic"]);
    let web = json!(["network", "user-input", "serialization"]);
    assert_eq!(
        query(root, &["trust-boundaries", user, "--json"])["trust_boundaries"],
        web
    );
    let original = std::fs::read(root.join(user)).unwrap();
    write(
        root,
        sibling,
        "package example; public @interface RestController {}",
    );
    run(root, &["index", "--no-semantic", "--no-features"]);
    assert_eq!(std::fs::read(root.join(user)).unwrap(), original);
    assert_eq!(
        query(root, &["trust-boundaries", user, "--json"])["trust_boundaries"],
        json!([])
    );
    for path in [
        "src/main/java/other/Other.java",
        "src/main/java/example/Explicit.java",
        "child/src/main/java/example/Child.java",
    ] {
        assert_eq!(
            query(root, &["trust-boundaries", path, "--json"])["trust_boundaries"],
            web,
            "{path}"
        );
    }
    run(root, &["map", "--json"]);
    let mapped = query(
        root,
        &["features-list", "--tag", "web-entrypoint", "--json"],
    );
    assert!(mapped["results"].as_array().unwrap().iter().all(|feature| {
        feature["files"]
            .as_array()
            .unwrap()
            .iter()
            .all(|file| file["path"] != user)
    }));
    std::fs::remove_file(root.join(sibling)).unwrap();
    run(root, &["index", "--no-semantic", "--no-features"]);
    assert_eq!(
        query(root, &["trust-boundaries", user, "--json"])["trust_boundaries"],
        web
    );
    run(root, &["map", "--json"]);
    let restored = query(
        root,
        &["features-list", "--tag", "web-entrypoint", "--json"],
    );
    assert!(
        restored["results"]
            .as_array()
            .unwrap()
            .iter()
            .any(|feature| feature["files"]
                .as_array()
                .unwrap()
                .iter()
                .any(|file| file["path"] == user))
    );
}

#[test]
fn oversized_java_skips_roles_and_refreshes_current_imports() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    maven_fixture(root);
    let controller = "src/main/java/example/Orders.java";
    let generated = "src/main/java/example/Generated.java";
    write(
        root,
        controller,
        "@org.springframework.web.bind.annotation.RestController class Orders {}",
    );
    write(
        root,
        generated,
        &format!(
            "import java.net.http.HttpClient; class Generated {{ HttpClient field; }} /*{}*/",
            "x".repeat(1_000_001)
        ),
    );
    run(root, &["index", "--no-semantic"]);
    assert_eq!(
        query(root, &["features-list", "--lang", "java", "--json"])["results"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        query(root, &["trust-boundaries", generated, "--json"])["trust_boundaries"],
        json!(["network", "external-api"])
    );
    write(
        root,
        generated,
        &format!(
            "import java.nio.file.Files; class Generated {{}} /*{}*/",
            "x".repeat(1_000_001)
        ),
    );
    run(root, &["map", "--json"]);
    assert_eq!(
        query(root, &["trust-boundaries", generated, "--json"])["trust_boundaries"],
        json!(["filesystem"])
    );
    assert_eq!(
        query(root, &["features-list", "--lang", "java", "--json"])["results"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn capped_java_discovery_retains_existing_maven_slices() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    maven_fixture(root);
    std::fs::remove_file(root.join("pom.xml")).unwrap();
    write(root, "zmodule/pom.xml", "<project/>");
    write(
        root,
        "zmodule/src/main/java/Orders.java",
        "@org.springframework.web.bind.annotation.RestController class Orders {}",
    );
    run(root, &["index", "--no-semantic"]);
    let before = query(root, &["features-list", "--lang", "java", "--json"])["results"].clone();
    assert_eq!(before.as_array().unwrap().len(), 1);
    std::fs::create_dir(root.join("a")).unwrap();
    for index in 0..50_000 {
        std::fs::write(root.join(format!("a/{index:05}.txt")), "").unwrap();
    }
    let output = Command::new(env!("CARGO_BIN_EXE_codesage"))
        .current_dir(root)
        .env("CODESAGE_WATCH", "0")
        .args(["map", "--json"])
        .output()
        .unwrap();
    let body: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(output.status.success(), "{body}");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("Maven discovery reached"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(body["removed"], 0);
    assert_eq!(
        query(root, &["features-list", "--lang", "java", "--json"])["results"],
        before
    );
}

#[test]
fn java_lexical_scopes_and_static_type_imports_feed_file_and_feature_output() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    maven_fixture(root);
    for (name, source) in [
        (
            "Generic",
            "import java.net.http.HttpClient; class Generic<HttpClient> { HttpClient field; }",
        ),
        (
            "Nested",
            "import java.net.http.HttpClient; class Nested { class HttpClient {} HttpClient field; }",
        ),
        (
            "Scope",
            "import org.springframework.web.bind.annotation.RestController; @RestController class Scope {} class Other { class RestController {} }",
        ),
        (
            "Direct",
            "import java.net.http.HttpClient.Builder; class Direct { Builder field; }",
        ),
        (
            "Static",
            "import static java.net.http.HttpClient.Builder; class Static { Builder field; }",
        ),
        (
            "StaticWildcard",
            "import static java.net.http.HttpClient.*; class StaticWildcard { Builder field; }",
        ),
        (
            "Unrelated",
            "import static example.Client.Builder; class Unrelated { Builder field; }",
        ),
        (
            "Method",
            "import static java.net.http.HttpClient.newBuilder; class Method { Object field; }",
        ),
    ] {
        write(root, &format!("src/main/java/example/{name}.java"), source);
    }
    run(root, &["index", "--no-semantic"]);
    for (name, expected) in [
        ("Generic", json!(["network", "external-api"])),
        ("Nested", json!(["network", "external-api"])),
        ("Scope", json!(["network", "user-input", "serialization"])),
        (
            "Direct",
            json!(["network", "external-api", "serialization"]),
        ),
        (
            "Static",
            json!(["network", "external-api", "serialization"]),
        ),
        (
            "StaticWildcard",
            json!(["network", "external-api", "serialization"]),
        ),
        ("Unrelated", json!([])),
        ("Method", json!(["network", "external-api"])),
    ] {
        let path = format!("src/main/java/example/{name}.java");
        assert_eq!(
            query(root, &["trust-boundaries", &path, "--json"])["trust_boundaries"],
            expected,
            "{path}"
        );
    }
    let features = query(root, &["features-list", "--lang", "java", "--json"]);
    let features = features["results"].as_array().unwrap();
    assert_eq!(features.len(), 2);
    let clients = features
        .iter()
        .find(|feature| {
            feature["tags"]
                .as_array()
                .unwrap()
                .contains(&json!("external-client"))
        })
        .unwrap();
    let client_files: Vec<_> = clients["files"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|file| file["role"] != "context")
        .map(|file| file["path"].as_str().unwrap())
        .collect();
    assert_eq!(
        client_files,
        [
            "src/main/java/example/Direct.java",
            "src/main/java/example/Static.java",
            "src/main/java/example/StaticWildcard.java"
        ]
    );
    let web = features
        .iter()
        .find(|feature| {
            feature["tags"]
                .as_array()
                .unwrap()
                .contains(&json!("web-entrypoint"))
        })
        .unwrap();
    assert_eq!(web["entry_path"], "src/main/java/example/Scope.java");
    assert_eq!(
        clients["trust_boundaries"],
        json!(["network", "external-api", "serialization"])
    );
}

#[test]
fn maven_spring_cli_exposes_role_boundaries_features_risk_and_map_refresh() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(
        root,
        ".codesage/config.toml",
        "[project]\nname = \"maven-fixture\"\n[embedding]\nmodel = \"jinaai/jina-embeddings-v2-base-code\"\ndevice = \"cpu\"\n[index]\nexclude_patterns = [\"src/main/java/excluded/**\"]\n",
    );
    write(
        root,
        "pom.xml",
        "<project><modelVersion>4.0.0</modelVersion><groupId>example</groupId><artifactId>orders</artifactId><version>1</version></project>",
    );
    let controller = "src/main/java/example/Orders.java";
    let repository = "src/main/java/example/Store.java";
    let client = "src/main/java/example/Remote.java";
    write(
        root,
        controller,
        "import org.springframework.web.bind.annotation.RestController;\nimport com.fasterxml.jackson.databind.ObjectMapper;\n@RestController class Orders {}\n",
    );
    write(
        root,
        repository,
        "import org.springframework.data.jpa.repository.JpaRepository;\ninterface Store extends JpaRepository<Order, Long> {}\n",
    );
    write(
        root,
        client,
        "import org.springframework.cloud.openfeign.FeignClient;\n@FeignClient(name=\"orders\") interface Remote {}\n",
    );
    write(
        root,
        "src/main/java/example/Logic.java",
        "@org.springframework.stereotype.Service class Logic {}\n",
    );
    write(
        root,
        "src/main/java/example/Config.java",
        "@org.springframework.context.annotation.Configuration class Config {}\n",
    );
    write(
        root,
        "src/main/java/example/Helper.java",
        "@org.springframework.stereotype.Component class Helper {}\n",
    );
    let plain = "src/main/java/controllers/PlainController.java";
    let unrelated = "src/main/java/example/Unrelated.java";
    let excluded = "src/main/java/excluded/Excluded.java";
    write(root, plain, "class PlainController {}\n");
    write(
        root,
        unrelated,
        "import example.RestController; @RestController class Unrelated {}\n",
    );
    write(
        root,
        excluded,
        "@org.springframework.web.bind.annotation.RestController class Excluded {}\n",
    );
    run(root, &["index", "--no-semantic"]);
    for (path, expected) in [
        (
            controller,
            json!(["network", "user-input", "serialization"]),
        ),
        (repository, json!(["database", "serialization"])),
        (client, json!(["network", "external-api", "serialization"])),
        (plain, json!([])),
        (unrelated, json!([])),
        (excluded, json!([])),
    ] {
        assert_eq!(
            query(root, &["trust-boundaries", path, "--json"])["trust_boundaries"],
            expected,
            "{path}"
        );
    }
    let risk = query(root, &["risk", controller, "--json"]);
    assert_eq!(
        risk["trust_boundaries"],
        json!(["network", "user-input", "serialization"])
    );
    assert!(risk["notes"].as_array().unwrap().iter().any(|note| {
        note.as_str()
            .unwrap()
            .contains("crosses 3 trust boundaries")
    }));
    let features = query(root, &["features-list", "--lang", "java", "--json"]);
    let features = features["results"].as_array().unwrap();
    assert_eq!(features.len(), 6, "{features:?}");
    for (role, expected) in [
        (
            "web-entrypoint",
            json!(["network", "user-input", "serialization"]),
        ),
        ("persistence-boundary", json!(["database", "serialization"])),
        (
            "external-client",
            json!(["network", "external-api", "serialization"]),
        ),
    ] {
        let feature = features
            .iter()
            .find(|feature| feature["tags"].as_array().unwrap().contains(&json!(role)))
            .unwrap();
        assert_eq!(feature["trust_boundaries"], expected);
    }
    write(
        root,
        controller,
        "import java.nio.file.Files; class Orders {}\n",
    );
    run(root, &["map", "--json"]);
    assert_eq!(
        query(root, &["trust-boundaries", controller, "--json"])["trust_boundaries"],
        json!(["filesystem"])
    );
    assert!(
        query(
            root,
            &["features-list", "--tag", "web-entrypoint", "--json"]
        )["results"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    run(root, &["index", "--no-semantic"]);
    let pom = std::fs::read(root.join("pom.xml")).unwrap();
    std::fs::remove_file(root.join("pom.xml")).unwrap();
    run(root, &["index", "--no-semantic"]);
    assert!(
        query(root, &["features-list", "--lang", "java", "--json"])["results"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    std::fs::write(root.join("pom.xml"), pom).unwrap();
    run(root, &["index", "--no-semantic"]);
    assert_eq!(
        query(root, &["features-list", "--lang", "java", "--json"])["results"]
            .as_array()
            .unwrap()
            .len(),
        5
    );
}
