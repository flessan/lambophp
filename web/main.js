/* Lambo PHP landing page - zero-dependency, no-build JavaScript.
 * Three small features:
 *   1. click-to-copy install command (with clipboard fallback)
 *   2. scripted terminal demo (real CLI output, typed animation, replayable)
 *   3. reveal-on-scroll + footer year
 * Everything honors prefers-reduced-motion.
 */
(function () {
  'use strict';

  document.documentElement.classList.add('js');

  var reducedMotion = window.matchMedia('(prefers-reduced-motion: reduce)').matches;

  /* ---------------------------------------------------------------
   * 1. Copy-to-clipboard
   * ------------------------------------------------------------- */
  function copyText(text) {
    if (navigator.clipboard && window.isSecureContext !== false) {
      return navigator.clipboard.writeText(text);
    }
    // Fallback for older browsers / non-secure contexts (file://).
    return new Promise(function (resolve, reject) {
      var ta = document.createElement('textarea');
      ta.value = text;
      ta.style.position = 'fixed';
      ta.style.opacity = '0';
      document.body.appendChild(ta);
      ta.select();
      try {
        document.execCommand('copy');
        resolve();
      } catch (err) {
        reject(err);
      } finally {
        document.body.removeChild(ta);
      }
    });
  }

  document.querySelectorAll('[data-copy]').forEach(function (button) {
    button.addEventListener('click', function () {
      var text = button.getAttribute('data-copy');
      var status = button.parentElement.querySelector('[data-copy-status]');
      copyText(text).then(function () {
        button.classList.add('copied');
        if (status) status.textContent = 'Install command copied to clipboard.';
        setTimeout(function () {
          button.classList.remove('copied');
          if (status) status.textContent = '';
        }, 1600);
      }).catch(function () {
        if (status) status.textContent = 'Copy failed - select the command and copy it manually.';
      });
    });
  });

  /* ---------------------------------------------------------------
   * 2. Terminal demo - the real output of the CLI as it ships today
   * ------------------------------------------------------------- */
  var SCRIPT = [
    { k: 'cmd', s: 'lambo init' },
    { k: 'ok', s: '\u2714 created `/home/dev/shop/lambo.yml`' },
    { k: 'head', s: '\u2500\u2500 Detected' },
    { k: 'out', s: '  framework      plain PHP' },
    { k: 'dim', s: '  \u00b7 [no ] composer.json' },
    { k: 'dim', s: '  \u00b7 [no ] artisan (Laravel\'s console entry point)' },
    { k: 'dim', s: '  \u00b7 [no ] wp-settings.php (WordPress core)' },
    { k: 'out', s: '  \u00b7 [yes] index.php' },
    { k: 'head', s: '\u2500\u2500 Configuration' },
    { k: 'out', s: '  php            stable' },
    { k: 'out', s: '  server         apache' },
    { k: 'out', s: '  document root  .' },
    { k: 'out', s: '  database       disabled' },

    { k: 'cmd', s: 'lambo doctor' },
    { k: 'ok', s: '\u2714 lambo home: /home/dev/.lambo' },
    { k: 'ok', s: '\u2714 downloads: curl is available' },
    { k: 'ok', s: '\u2714 catalogue: 2 PHP, 0 Apache and 1 MariaDB releases for linux-x64' },
    { k: 'fail', s: '\u2716 php: no PHP version is installed' },
    { k: 'dim', s: '  hint: lambo php install stable' },
    { k: 'ok', s: '\u2714 ports: server.port: 8080 is free' },
    { k: 'ok', s: '\u2714 project: shop: plain PHP served from /home/dev/shop' },

    { k: 'cmd', s: 'lambo up --no-browser' },
    { k: 'head', s: '\u2500\u2500 Starting shop' },
    { k: 'fail', s: 'error: PHP stable failed to start: download failed: \u2026 no checksum' },
    { k: 'fail', s: 'is available for this download, so it cannot be verified.' },
    { k: 'dim', s: '  possible causes:' },
    { k: 'dim', s: '    - `lambo php list-versions` shows what is available for linux-x64' },
    { k: 'dim', s: '    - a release without a pinned checksum is never run' },
    { k: 'dim', s: '  next: lambo php install stable, or `lambo init --php <version>`' },

    { k: 'comment', s: '# that failure is the point: the shipped catalogue pins no checksums,' },
    { k: 'comment', s: '# and Lambo will not run a download it cannot verify. Pin one and it runs.' },
    { k: 'blank', s: '' }
  ];

  var body = document.getElementById('term-body');
  var replay = document.getElementById('term-replay');
  var runToken = 0; // incremented to abort a running animation

  function makeLine(kind) {
    var div = document.createElement('div');
    if (kind !== 'blank') div.className = 't-' + kind;
    return div;
  }

  function renderStatic() {
    body.textContent = '';
    SCRIPT.forEach(function (line) {
      var div = makeLine(line.k);
      div.textContent = line.s;
      body.appendChild(div);
    });
  }

  function delay(ms) {
    return new Promise(function (resolve) { setTimeout(resolve, ms); });
  }

  function play() {
    if (reducedMotion || !body) { renderStatic(); return; }
    var token = ++runToken;

    (async function () {
      body.textContent = '';
      for (var i = 0; i < SCRIPT.length; i++) {
        if (token !== runToken) return; // a replay took over

        var line = SCRIPT[i];
        var div = makeLine(line.k);
        body.appendChild(div);

        if (line.k === 'cmd') {
          if (i > 0) await delay(850); // pause between commands
          var cursor = document.createElement('span');
          cursor.className = 'cursor';
          div.appendChild(cursor);
          for (var c = 0; c < line.s.length; c++) {
            if (token !== runToken) return;
            cursor.before(document.createTextNode(line.s[c]));
            await delay(26 + Math.random() * 34); // human-ish typing
          }
          cursor.remove();
          await delay(420);
        } else if (line.k === 'blank') {
          // just a spacer line
        } else {
          div.textContent = line.s;
          await delay(52);
        }
      }
      // Final resting cursor.
      var end = makeLine('cmd');
      var endCursor = document.createElement('span');
      endCursor.className = 'cursor';
      end.appendChild(endCursor);
      body.appendChild(end);
    })();
  }

  if (body) {
    var started = false;
    if ('IntersectionObserver' in window && !reducedMotion) {
      var obs = new IntersectionObserver(function (entries) {
        if (!started && entries[0].isIntersecting) {
          started = true;
          play();
          obs.disconnect();
        }
      }, { threshold: 0.3 });
      obs.observe(body);
    } else {
      started = true;
      play();
    }
    if (replay) replay.addEventListener('click', play);
  }

  /* ---------------------------------------------------------------
   * 3. Reveal-on-scroll + footer year
   * ------------------------------------------------------------- */
  var revealEls = document.querySelectorAll('.reveal');
  if ('IntersectionObserver' in window && !reducedMotion) {
    var revealObs = new IntersectionObserver(function (entries) {
      entries.forEach(function (entry) {
        if (entry.isIntersecting) {
          entry.target.classList.add('is-visible');
          revealObs.unobserve(entry.target);
        }
      });
    }, { threshold: 0.12 });
    revealEls.forEach(function (el) { revealObs.observe(el); });
  } else {
    revealEls.forEach(function (el) { el.classList.add('is-visible'); });
  }

  var year = document.getElementById('year');
  if (year) year.textContent = String(new Date().getFullYear());
})();
