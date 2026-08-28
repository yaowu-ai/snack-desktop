use crate::web::ALLOWED_WEB_ORIGINS;

const INITIALIZATION_SCRIPT_TEMPLATE: &str = r#"
(function () {
  var allowedOrigins = __SNACK_ALLOWED_ORIGINS__;
  if (allowedOrigins.indexOf(window.location.origin) === -1) return;
  if (window.__SNACK_EARLY_CHUNK_GUARD_INSTALLED__) return;
  window.__SNACK_EARLY_CHUNK_GUARD_INSTALLED__ = true;

  function describeFailure(value) {
    if (typeof value === 'string') return value;
    if (!value || typeof value !== 'object') return '';
    return [value.name, value.message, value.stack].filter(Boolean).join(' ');
  }

  function isKnownChunkFailure(value) {
    return /ChunkLoadError|Loading (?:CSS )?chunk [^ ]+ failed|Failed to fetch dynamically imported module|Importing a module script failed|Failed to load module script/i.test(describeFailure(value));
  }

  function isNextStaticResource(target) {
    var source = target && (target.src || target.href);
    if (!source) return false;
    try {
      var resourceUrl = new URL(source, window.location.href);
      return resourceUrl.origin === window.location.origin && resourceUrl.pathname.indexOf('/_next/static/') === 0;
    } catch (error) {
      return false;
    }
  }

  function rememberFailure(reason) {
    if (!window.__SNACK_PENDING_WEB_RUNTIME_FAILURE__) {
      window.__SNACK_PENDING_WEB_RUNTIME_FAILURE__ = reason;
    }
  }

  window.addEventListener('error', function (event) {
    if (isNextStaticResource(event.target) || isKnownChunkFailure(event.error || event.message)) {
      rememberFailure('chunk-load-error');
    }
  }, true);
  window.addEventListener('unhandledrejection', function (event) {
    if (isKnownChunkFailure(event.reason)) {
      rememberFailure('dynamic-import-error');
    }
  });
})();
"#;

pub(crate) fn initialization_script() -> String {
    let allowed_origins =
        serde_json::to_string(ALLOWED_WEB_ORIGINS).expect("Snack Web origins must serialize");
    INITIALIZATION_SCRIPT_TEMPLATE.replace("__SNACK_ALLOWED_ORIGINS__", &allowed_origins)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scopes_the_early_chunk_guard_to_snack_origins() {
        let script = initialization_script();

        for origin in ALLOWED_WEB_ORIGINS {
            assert!(script.contains(origin));
        }
        assert!(script.contains("window.__SNACK_PENDING_WEB_RUNTIME_FAILURE__"));
        assert!(script.contains("/_next/static/"));
        assert!(script.contains("unhandledrejection"));
        assert!(!script.contains("__SNACK_ALLOWED_ORIGINS__"));
    }
}
