use std::collections::BTreeSet;

use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Framework {
    Next,
    Nuxt,
    SvelteKit,
    RemixReactRouter,
    Astro,
}

impl Framework {
    pub(crate) fn route_globs(self) -> &'static [&'static str] {
        match self {
            Self::Next => &[
                "app/**/{page,layout,route,template,error,loading,not-found}.{ts,tsx,js,jsx}",
                "src/app/**/{page,layout,route,template,error,loading,not-found}.{ts,tsx,js,jsx}",
                "pages/**/*.{ts,tsx,js,jsx}",
                "src/pages/**/*.{ts,tsx,js,jsx}",
                "middleware.{ts,js}",
                "src/middleware.{ts,js}",
                "app/**/default.{ts,tsx}",
                "src/app/**/default.{ts,tsx}",
            ],
            Self::Nuxt => &[
                "server/api/**/*.{ts,js}",
                "server/routes/**/*.{ts,js}",
                "middleware/**/*.{ts,js}",
                "plugins/**/*.{ts,js}",
            ],
            Self::SvelteKit => &["src/routes/**/+*.{ts,js}"],
            Self::RemixReactRouter => &[
                "app/routes/**/*.{ts,tsx,js,jsx}",
                "app/root.{ts,tsx,js,jsx}",
            ],
            Self::Astro => &["src/pages/**/*.{ts,js}"],
        }
    }

    pub(crate) fn framework_called_exports(self) -> BTreeSet<String> {
        let names: &[&str] = match self {
            Self::Next => &[
                "default",
                "GET",
                "POST",
                "PUT",
                "DELETE",
                "PATCH",
                "HEAD",
                "OPTIONS",
                "metadata",
                "generateMetadata",
                "generateStaticParams",
                "middleware",
                "config",
            ],
            Self::Nuxt => &["default"],
            Self::SvelteKit => &[
                "load", "actions", "GET", "POST", "PUT", "DELETE", "PATCH", "HEAD", "OPTIONS",
            ],
            Self::RemixReactRouter => &["default", "loader", "action", "meta", "links"],
            Self::Astro => &[
                "default",
                "GET",
                "POST",
                "PUT",
                "DELETE",
                "PATCH",
                "HEAD",
                "OPTIONS",
                "ALL",
                "getStaticPaths",
                "prerender",
            ],
        };
        names.iter().map(|name| (*name).to_string()).collect()
    }

    fn dependency_names(self) -> &'static [&'static str] {
        match self {
            Self::Next => &["next"],
            Self::Nuxt => &["nuxt"],
            Self::SvelteKit => &["@sveltejs/kit"],
            Self::RemixReactRouter => &[
                "@remix-run/react",
                "@remix-run/node",
                "@remix-run/dev",
                "@react-router/dev",
            ],
            Self::Astro => &["astro"],
        }
    }

    fn script_commands(self) -> &'static [&'static str] {
        match self {
            Self::Next => &["next"],
            Self::Nuxt => &["nuxt", "nuxi"],
            Self::SvelteKit => &["svelte-kit", "vite"],
            Self::RemixReactRouter => &["remix", "react-router"],
            Self::Astro => &["astro"],
        }
    }
}

pub(crate) fn detected_route_frameworks(manifest: &Value) -> BTreeSet<Framework> {
    [
        Framework::Next,
        Framework::Nuxt,
        Framework::SvelteKit,
        Framework::RemixReactRouter,
        Framework::Astro,
    ]
    .into_iter()
    .filter(|framework| framework_is_enabled(manifest, *framework))
    .collect()
}

fn framework_is_enabled(manifest: &Value, framework: Framework) -> bool {
    if has_manifest_dependency(
        manifest,
        &["dependencies", "optionalDependencies"],
        framework.dependency_names(),
    ) {
        return true;
    }

    has_manifest_dependency(manifest, &["devDependencies"], framework.dependency_names())
        && has_matching_framework_script(manifest, framework)
}

fn has_manifest_dependency(manifest: &Value, sections: &[&str], names: &[&str]) -> bool {
    sections.iter().any(|section| {
        manifest
            .get(*section)
            .and_then(Value::as_object)
            .is_some_and(|deps| names.iter().any(|name| deps.contains_key(*name)))
    })
}

fn has_matching_framework_script(manifest: &Value, framework: Framework) -> bool {
    let Some(scripts) = manifest.get("scripts").and_then(Value::as_object) else {
        return false;
    };
    scripts
        .values()
        .filter_map(Value::as_str)
        .any(|script| script_has_command(script, framework.script_commands()))
}

fn script_has_command(script: &str, commands: &[&str]) -> bool {
    script
        .split(|ch: char| !(ch.is_ascii_alphanumeric() || matches!(ch, '@' | '/' | '-' | '_')))
        .filter(|token| !token.is_empty())
        .any(|token| commands.contains(&token))
}
