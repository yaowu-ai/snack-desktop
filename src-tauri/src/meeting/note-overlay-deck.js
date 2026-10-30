(function () {
  'use strict';

  /** Presentation only: capture controls always remain bound to the main state. */
  window.createSnackRecordingDeck = function (options) {
    var root = document.getElementById('deck');
    var previousCard = document.getElementById('previous-card');
    var previousFull = document.getElementById('previous-full');
    var currentContent = document.getElementById('current-content');
    var notice = document.getElementById('handoff-notice');
    var media = window.matchMedia('(prefers-reduced-motion: reduce)');
    var current = {};
    var identity = '';
    var previousIdentity = '';
    var view = 'recording';
    var direction = 1;
    var generation = 0;
    var timers = [];
    var animations = [];
    var consumed = new Set();
    var receiptVisible = false;


    function key(value) {
      return value ? value.recordingId + ':' + (value.resultAttempt || 'capture') : '';
    }

    function later(callback, delay) {
      var version = generation;
      timers.push(setTimeout(function () { if (version === generation) callback(); }, delay));
    }

    function cancelMotion() {
      generation += 1;
      timers.forEach(clearTimeout);
      timers = [];
      animations.forEach(function (animation) { animation.cancel(); });
      animations = [];
      options.capsule.style.willChange = previousCard.style.willChange = '';
    }

    function pose(offset, angle, depth, scale) {
      return 'translateY(' + direction * offset + 'px) translateZ(' + depth + 'px) rotateX(' + direction * angle + 'deg) scale(' + scale + ')';
    }

    function front() { return pose(0, 0, 0, 1); }
    function receipt() { return pose(84, 0, 0, 1); }
    function duration(value) { return media.matches ? 80 : value; }

    function animate(element, frames, milliseconds) {
      if (!element.animate) return;
      element.style.willChange = 'transform, opacity';
      var animation = element.animate(frames, {
        duration: duration(milliseconds), easing: 'cubic-bezier(.22,1,.36,1)', fill: 'both'
      });
      animations.push(animation);
      animation.finished.then(function () { element.style.willChange = ''; }, function () {});
    }

    function mode(next) {
      previousCard.dataset.mode = next;
      previousFull.hidden = next !== 'full';
      notice.hidden = next !== 'receipt';
    }

    function labels() {
      var title = window.snackText('backgroundTranscriptionTitle');
      document.getElementById('previous-label').textContent = title;
      document.getElementById('handoff-title').textContent = title;
      document.getElementById('handoff-description').textContent = window.snackText('backgroundTranscriptionDescription');
    }

    function layout() {
      var nativeStack = current.phase === 'recording' && Boolean(current.previous);
      var presenting = nativeStack && (view === 'handoff' || receiptVisible);
      // Keep the opaque fallback until native restores the single-card backdrop.
      document.documentElement.dataset.deckActive = String(nativeStack);
      root.dataset.deckView = view;
      previousCard.hidden = !presenting;
      previousCard.inert = !presenting;
      currentContent.hidden = false;
      currentContent.inert = presenting && view === 'handoff';
      previousFull.inert = true;
      options.capsule.style.pointerEvents = presenting && view === 'handoff' ? 'none' : '';
      previousCard.style.pointerEvents = 'none';
      if (!presenting) return singleCard();
      if (view === 'handoff') return;
      options.capsule.style.opacity = previousCard.style.opacity = '1';
      options.capsule.style.transform = front();
      previousCard.style.transform = receipt();
      options.capsule.style.zIndex = '3';
      previousCard.style.zIndex = '1';
      mode('receipt');
    }

    function singleCard() {
      view = 'recording';
      root.dataset.deckView = view;
      previousCard.hidden = true;
      mode('hidden');
      currentContent.hidden = false;
      currentContent.inert = false;
      options.capsule.style.transform = '';
      options.capsule.style.opacity = '';
      options.capsule.style.zIndex = '';
      options.capsule.style.pointerEvents = '';
    }

    function finishReceipt() {
      if (!receiptVisible) return;
      cancelMotion();
      receiptVisible = false;
      layout();
    }

    function finishHandoff() {
      cancelMotion();
      view = 'recording';
      receiptVisible = Boolean(current.previous);
      layout();
      if (receiptVisible) later(finishReceipt, 3000);
    }

    function handoff() {
      cancelMotion();
      view = 'handoff';
      receiptVisible = false;
      layout();
      mode('full');
      previousCard.style.zIndex = '3';
      options.capsule.style.zIndex = '1';
      var enter = pose(30, -18, -76, .92);
      var retire = pose(-14, 18, -46, .92);
      var frames = [{ transform: front() }, { transform: retire, offset: .45 }, { transform: receipt() }];
      if (media.matches) frames = [{ transform: receipt(), opacity: 0 }, { transform: receipt(), opacity: 1 }];
      animate(previousCard, frames, 560);
      animate(options.capsule, [{ transform: media.matches ? front() : enter }, { transform: front() }], 560);
      later(function () { previousCard.style.zIndex = '1'; options.capsule.style.zIndex = '3'; mode('receipt'); }, duration(560) * .46);
      later(finishHandoff, duration(560));
    }

    function update(next) {
      var nextIdentity = key(next);
      var nextPrevious = next.phase === 'recording' ? key(next.previous) : '';
      var nextDirection = next.stackAbove ? -1 : 1;
      if (identity !== nextIdentity || previousIdentity !== nextPrevious || direction !== nextDirection) {
        cancelMotion(); view = 'recording'; receiptVisible = false;
      }
      identity = nextIdentity; previousIdentity = nextPrevious; direction = nextDirection;
      current = next;
      document.documentElement.dataset.stackAbove = String(direction === -1);
      document.documentElement.style.setProperty('--capsule-top', (next.capsuleTop === 97 ? 97 : 13) + 'px');
      labels();
      if (!nextPrevious) { cancelMotion(); receiptVisible = false; }
      layout();
      consumeHandoff(next);
    }

    function consumeHandoff(next) {
      var id = next.handoffNoticeId;
      if (!id || !previousIdentity || consumed.has(id)) return;
      consumed.add(id);
      if (consumed.size > 64) consumed.delete(consumed.values().next().value);
      handoff();
    }

    document.addEventListener('visibilitychange', function () {
      if (document.hidden) { cancelMotion(); receiptVisible = false; view = 'recording'; layout(); }
    });
    window.addEventListener('pagehide', cancelMotion);
    return { update: update };
  };
})();
