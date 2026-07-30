# Privacy

Carrel is a local-first desktop app. Your library, reading progress, highlights,
and settings stay on your device.

## Usage analytics (opt-in, off by default)

To understand how many people use Carrel, the app can send a single anonymous
event — `app_started` — once per launch, but **only after you explicitly opt in**.
Nothing is sent until you choose "Enable" on the first-run prompt or in Settings.

**Processor:** [Aptabase](https://aptabase.com) (EU region, `eu.aptabase.com`),
acting as our data processor. **Legal basis:** your consent (GDPR Art. 6(1)(a)),
which you can withdraw at any time.

**What is sent** (by the Aptabase SDK, with the `app_started` event):
operating system name and version, Carrel's app version, your locale, the webview
engine name/version, a short-lived random session id, and two pieces of
non-identifying technical metadata — a debug-build flag (`isDebug`) and the
Aptabase SDK's own name and version (`sdkVersion`).

**What is never sent:** book titles, authors, file paths, library contents,
reading progress, highlights, or any stable identifier for you or your install.

**Identity:** Carrel transmits no user or install identifier. Aptabase derives a
short-lived, salted **pseudonymous** identifier server-side from your IP address
and user agent to approximate unique-client counts. Data is retained by Aptabase
per their [privacy policy](https://aptabase.com/legal/privacy).

**What we learn:** approximate active desktop clients per day/month and the
spread of operating systems and app versions. Because there is no install
identifier, these are approximations of *active clients*, not exact user or
install counts. Usage that happens only through a long-running instance's web UI
or OPDS server (without a fresh desktop launch) is not counted.

## How to opt out

Analytics are off until you opt in. To turn them off after enabling:
**Settings → (General) → "Send anonymous usage statistics"** and switch it off.
This stops all future events; an event already queued in the current session may
still be sent, so the change takes full effect no later than the next launch.
