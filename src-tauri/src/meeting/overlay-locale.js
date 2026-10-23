(function () {
  var snapshot = window.__SNACK_LOCALE__;
  window.snackText = function (key) {
    return snapshot.messages[key] || snapshot.messages.unavailable;
  };
  window.snackReminderDescription = function () { return snapshot.reminderDescription; };
  window.setSnackLocale = function (next) {
    snapshot = next;
    document.documentElement.lang = next.locale;
    document.querySelectorAll('[data-i18n]').forEach(function (element) {
      element.textContent = window.snackText(element.dataset.i18n);
    });
    document.querySelectorAll('[data-i18n-label]').forEach(function (element) {
      element.setAttribute('aria-label', window.snackText(element.dataset.i18nLabel));
      element.setAttribute('title', window.snackText(element.dataset.i18nLabel));
    });
    if (window.renderSnackLocale) window.renderSnackLocale();
  };
  window.setSnackLocale(snapshot);
})();
