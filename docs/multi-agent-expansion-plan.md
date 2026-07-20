---
provenance: ai_authored
provenance_metadata:
  agent: claude-opus-4-8
  session_date: 2026-07-20
  human_validated: false
  session_context: "Rozbudowa multi-agentów — post-launch roadmap dla governed A2A delegation"
---

# Multi-agent — plan rozbudowy

**North star.** Delegację przez A2A ma wielu; deterministycznego nadzorcę nad siecią agentów — prawie
nikt. To jest różnicownik Keltera. Każda rozbudowa multi-agentów ma **pogłębiać governance nad siecią**,
nie rozmywać go w kolejne featury orkiestracji.

## Stan obecny (zweryfikowany w kodzie)

- **A2A dwukierunkowe.** Klient — deleguj OUT (`A2aClient` / `RemoteAgent` / `AgentCard`, `crates/a2a`); serwer — inbound, Kelter jako czyjś sub-agent (ADR 0013).
- **Delegacja = zwalidowana akcja.** Każdy zarejestrowany agent → akcja `delegate_to_<name>` w action space (jako `ActionSource`). Model proponuje, kernel waliduje i wykonuje: send → journal → poll-to-terminal → atrybucja (`attribute_agent`: kto, ile ms).
- **Governed + durable** (ADR 0009). Delegować można tylko do zarejestrowanych; wszystko journalowane; po restarcie Kelter podpina się do trwającego zdalnego taska zamiast re-delegować.
- **Tryb plan** (ADR 0010). DAG z węzłami `NodeType::A2a`, walidowany z góry (węzeł „a2a" musi być realną akcją `delegate_to_`).
- **Ograniczenia dziś:** delegacje **sekwencyjne** (poll-to-terminal per krok); outbound v1 = zamknięte pod-zadanie bez interakcji w trakcie; **brak** budżetów / limitu głębokości / wykrywania cykli; agenci rejestrowani **statycznie**; brak retry/fallback; zdalny agent **opaque** do zakończenia.

## Priorytety

### P1 — Równoległy fan-out  *(idzie w parze z P2)*

Sens multi-agentów to często równoległe rozłożenie pracy. DAG w trybie plan **już wyraża** niezależność
węzłów — brakuje **współbieżnego wykonania** niezależnych gałęzi (z poszanowaniem zależności), dalej
governed i journalowanego per gałąź. Wymaga: eventów niosących wymiar gałęzi/agenta (dziś bucketowane po
`run_id`+`seq` — dołożyć `branch`/delegation-id), tak by timeline dało się rozłożyć na tory. Bez tego
multi-agent jest wolny i pół-użyteczny.

### P2 — Governed delegation: budżety, limit głębokości, wykrywanie cykli

To jest killer feature, nie sam fan-out. Prawie nikt nie pilnuje, żeby A→B→A nie poszło w nieskończoność,
żeby nie wystrzeliło 200 delegacji, żeby był limit głębokości i budżet per-agent. Deterministyczny kernel
jest do tego stworzony — egzekwuje limity tak, jak egzekwuje action space. Równoległość **czyni te limity
konieczne** (łatwiej o runaway), więc P1 i P2 dowozi się **razem**.

### P3 — Ujednolicić trust agentów z modelem skilli

Dziś agenci rejestrowani statycznie. Krok: dynamiczne odkrywanie + **ten sam wzorzec co skille** — agent
untrusted by default, polityka trusted/all/ask, człowiek zatwierdza, zanim model może delegować do
świeżego agenta. Symetria: „nieznany agent = jak untrusted skill; kernel nie da modelowi mu zlecić, póki
nie powiesz OK". Maszyneria (trust/policy/ask) w większości już jest po stronie skilli — do przeniesienia.

### P4 — Odporność: retry / fallback / circuit-break

Zdalny agent pada albo timeoutuje → dziś po prostu błąd. Governed failure handling: fallback na innego
agenta, ograniczone retry, circuit-break przy powtarzalnej awarii. Znów pod tezę — nadzorca zarządza
porażką, nie tylko sukcesem.

### P5 — (później, cięższe) Interaktywna delegacja wielo-turowa + streaming

Dziś outbound v1 = zamknięte pod-zadanie, bez wymiany w trakcie. Wielo-turowa delegacja wymaga
pauzowanego-czekającego-na-agenta stanu z durability — czyli **dokładnie tej samej maszynerii co pauza
„ask" człowiek-w-pętli**. Więc stanieje po tamtym. Plus: streaming postępu zdalnego agenta do timeline
(dziś opaque do końca).

## Sekwencja / timing

**Post-launch.** Do Product Hunta obecne A2A — governed, durable, atrybuowane delegacje — to już
różnicujący story; rozbudowa to „dokąd to idzie", nie warunek startu. Nie rozszerzać multi-agentów przed
launchem (masz dość: skille + core).

Kolejność: **P1+P2 jako pierwszy prompt** — spina „użyteczne" z „governed", i to jest zdanie, które
sprzedaje Keltera: *sieć agentów współpracująca pod deterministycznym nadzorcą — ograniczona, audytowalna,
odtwarzalna — zamiast wolnej amerykanki.* Dalej P3 → P4 fast-follow. P5 po pauzie „ask".
