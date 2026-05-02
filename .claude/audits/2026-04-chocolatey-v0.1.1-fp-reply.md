# Chocolatey moderator reply — anodizer 0.1.1

Drop-in reply for <https://community.chocolatey.org/packages/anodizer/0.1.1>.

VirusTotal report (windows-amd64.zip):
<https://www.virustotal.com/gui/file/d0177ea8564f6e33a971ab54097fb9efe07eb4bb81e9e3ad535801c0b96bb77c/detection>

---

Hi moderators —

Re: VirusTotal flags on anodizer-0.1.1-windows-amd64.zip (SHA-256 d0177ea8564f6e33a971ab54097fb9efe07eb4bb81e9e3ad535801c0b96bb77c).

These are false positives on the Telegram-announcer feature, not malware.

Detection breakdown (7/62):
  - Kaspersky : HEUR:Backdoor.Win64.TGBot.gen
  - Antiy-AVL : Trojan[Backdoor]/Win64.TGBot
  - Rising : Backdoor.TGBot!8.171AD
  - Alibaba : Backdoor:Application/Generic.cb284b2e
  - DeepInstinct : MALICIOUS (static ML, opaque)
  - Sophos : Generic Reputation PUA (new-file reputation, not a malware verdict)
  - Trellix ENS : Artemis!36794E071F4F (reputation heuristic)

Three of the seven explicitly attribute "TGBot." TGBot is a Telegram-Bot-API based RAT family, and its static signatures key on the standard Telegram bot API call shape: api.telegram.org/bot<token>/sendMessage with chat_id and parse_mode parameters.

anodizer is an open-source Rust release-management tool (a Rust port of GoReleaser). Like GoReleaser, it ships an `announce` stage with publishers for Slack, Discord, Mattermost, Reddit, Bluesky, Mastodon, Twitter, SMTP, Webhook, and Telegram. The Telegram announcer is the cause of the family attribution — the binary contains string and code patterns matching exactly the API surface those signatures fingerprint, because that *is* the public Telegram Bot API.

Source for review:
  - Repository : https://github.com/tj-smith47/anodizer
  - Announcer  : crates/stage-announce/src/telegram.rs
  - Release    : https://github.com/tj-smith47/anodizer/releases/tag/v0.1.1
  - Build      : reproducible from tag v0.1.1, MIT-licensed, Rust + cargo

This false-positive class is well-documented for any Windows tool that integrates Telegram notifications (release tools, monitoring agents, deploy bots, etc.).

Happy to provide additional information, a build log, or anything else useful for the review. Thanks for your time.
