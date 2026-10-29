(function () {
  'use strict';

  /** Presentation only: capture controls always remain bound to the main state. */
  window.createSnackRecordingDeck = function (options) {
    var root = document.getElementById('deck');
    var previousCard = document.getElementById('previous-card');
    var previousFull = document.getElementById('previous-full');
    var previousPeek = document.getElementById('previous-peek');
    var currentContent = document.getElementById('current-content');
    var recordingPeek = document.getElementById('recording-peek');
    var notice = document.getElementById('handoff-notice');
    var previousOpen = document.getElementById('previous-open');
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
    var closingReceipt = false;
    var pending = false;
    var phaseKeys = {
      transcribing: 'transcribing', completed: 'transcriptionCompleted',
      failed: 'transcriptionFailed', recording_failed: 'recordingFailed',
      transcription_paused: 'transcriptionPaused', ended: 'recordingEnded'
    };

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
      closingReceipt = false;
      options.capsule.style.willChange = previousCard.style.willChange = '';
    }

    function pose(offset, angle, depth, scale) {
      return 'translateY(' + direction * offset + 'px) translateZ(' + depth + 'px) rotateX(' + direction * angle + 'deg) scale(' + scale + ')';
    }

    function front() { return pose(0, 0, 0, 1); }
    function back() { return pose(24, media.matches ? 0 : -4, media.matches ? 0 : -32, .95); }
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
      previousPeek.hidden = next !== 'back';
      notice.hidden = next !== 'receipt';
    }

    function labels() {
      var previous = current.previous || {};
      var phaseKey = phaseKeys[previous.phase] || 'recordingEnded';
      document.getElementById('previous-label').textContent = window.snackText(phaseKey);
      document.getElementById('previous-peek-label').textContent = window.snackText('previousTranscription') + ' · ' + window.snackText(phaseKey);
      document.getElementById('recording-peek-label').textContent = window.snackText(current.paused ? 'paused' : 'recording');
      document.getElementById('recording-peek-time').textContent = document.getElementById('duration').textContent;
      document.getElementById('handoff-title').textContent = window.snackText('backgroundTranscriptionTitle');
      document.getElementById('handoff-description').textContent = window.snackText('backgroundTranscriptionDescription');
      previousOpen.disabled = pending;
      updateStatusIcon(previous.phase);
    }

    function updateStatusIcon(phase) {
      var icon = document.getElementById('previous-status-icon');
      var name = phase === 'transcribing' ? 'transcribing' : phase === 'completed' ? 'success' : 'failure';
      icon.hidden = phase === 'ended' || phase === 'transcription_paused';
      icon.src = '/note-assets/' + name + '.svg';
      icon.classList.toggle('loading', phase === 'transcribing');
    }

    function layout() {
      var hasPrevious = current.phase === 'recording' && Boolean(current.previous);
      document.documentElement.dataset.deckActive = String(hasPrevious);
      root.dataset.deckView = view;
      previousCard.hidden = !hasPrevious;
      currentContent.hidden = hasPrevious && view === 'previous';
      recordingPeek.hidden = !hasPrevious || view !== 'previous';
      currentContent.inert = view === 'handoff' || (hasPrevious && view === 'previous');
      previousFull.inert = view !== 'previous';
      options.capsule.style.pointerEvents = view === 'handoff' ? 'none' : '';
      previousCard.style.pointerEvents = view === 'handoff' ? 'none' : '';
      if (!hasPrevious) return singleCard();
      if (view === 'handoff' || closingReceipt) return;
      options.capsule.style.opacity = previousCard.style.opacity = '1';
      options.capsule.style.transform = view === 'previous' ? back() : front();
      previousCard.style.transform = view === 'previous' ? front() : receiptVisible ? receipt() : back();
      options.capsule.style.zIndex = view === 'previous' ? '1' : '3';
      previousCard.style.zIndex = view === 'previous' ? '3' : '1';
      mode(view === 'previous' ? 'full' : receiptVisible ? 'receipt' : 'back');
    }

    function singleCard() {
      view = 'recording';
      root.dataset.deckView = view;
      currentContent.hidden = false;
      currentContent.inert = false;
      recordingPeek.hidden = true;
      options.capsule.style.transform = '';
      options.capsule.style.opacity = '';
      options.capsule.style.zIndex = '';
      options.capsule.style.pointerEvents = '';
    }

    function finishReceipt() {
      if (!receiptVisible || view !== 'recording') return;
      cancelMotion();
      receiptVisible = false;
      if (media.matches) { layout(); return; }
      closingReceipt = true;
      animate(previousCard, [{ transform: receipt() }, { transform: back() }], 220);
      later(function () { cancelMotion(); layout(); }, 220);
    }

    function finishHandoff() {
      cancelMotion();
      view = 'recording';
      receiptVisible = current.previous && current.previous.phase === 'transcribing';
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

    function changeFocus(next) {
      if (!current.previous || view === 'handoff' || next === view) return;
      cancelMotion();
      receiptVisible = false;
      var showingPrevious = next === 'previous';
      view = next;
      layout();
      animate(options.capsule, [{ transform: showingPrevious ? front() : back() }, { transform: showingPrevious ? back() : front() }], 360);
      animate(previousCard, [{ transform: showingPrevious ? back() : front() }, { transform: showingPrevious ? front() : back() }], 360);
      later(function () { cancelMotion(); layout(); }, duration(360));
    }

    function update(next, isPending) {
      var nextIdentity = key(next);
      var nextPrevious = next.phase === 'recording' ? key(next.previous) : '';
      var nextDirection = next.stackAbove ? -1 : 1;
      if (identity !== nextIdentity || previousIdentity !== nextPrevious || direction !== nextDirection) {
        cancelMotion(); view = 'recording'; receiptVisible = false;
      }
      identity = nextIdentity; previousIdentity = nextPrevious; direction = nextDirection;
      current = next; pending = Boolean(isPending);
      document.documentElement.dataset.stackAbove = String(direction === -1);
      document.documentElement.style.setProperty('--capsule-top', (next.capsuleTop === 97 ? 97 : 13) + 'px');
      labels();
      if (!nextPrevious || (receiptVisible && next.previous.phase !== 'transcribing')) { cancelMotion(); receiptVisible = false; }
      layout();
      consumeHandoff(next);
    }

    function consumeHandoff(next) {
      var id = next.handoffNoticeId;
      if (!id || !previousIdentity || next.previous.phase !== 'transcribing' || consumed.has(id)) return;
      consumed.add(id);
      if (consumed.size > 64) consumed.delete(consumed.values().next().value);
      handoff();
    }

    previousPeek.addEventListener('click', function () { changeFocus('previous'); });
    recordingPeek.addEventListener('click', function () { changeFocus('recording'); });
    previousOpen.addEventListener('click', function () {
      if (!pending && current.previous) options.openPrevious(Object.assign({}, current.previous));
    });
    document.addEventListener('visibilitychange', function () {
      if (document.hidden) { cancelMotion(); receiptVisible = false; view = 'recording'; layout(); }
    });
    window.addEventListener('pagehide', cancelMotion);
    return { update: update };
  };
})();
