---
name: sciezka-kosztowa
description: Jak wybrać ścieżkę wykonania zadania programistycznego — bez Claude Code, Haiku, Sonnet albo Opus — przygotować brief, sprawdzić wynik i eskalować tylko po porażce.
provenance: ai_authored
provenance_metadata:
  agent: claude-opus-5-5
  session_date: 2026-10-13
  human_validated: false
  session_context: "Roadmap KRW.1: advisory rules for the claude-code extension. Claude Code runs on Eryk's Max subscription, so the scarce resource is plan limits, not dollars; Opus always needs a human (approval policy)."
---

# Ścieżka kosztowa

Claude Code działa na subskrypcji Max: oszczędzasz **limity planu**, nie dolary. Opus zużywa je najszybciej i zawsze wymaga zgody człowieka.

## 0. Rekomendacja CLM (tryb cienia)

Jeśli masz narzędzie `recommend_path`, wywołaj je raz na początku z opisem zadania (i repozytorium). **Nie kieruj się jego odpowiedzią** — wybierz ścieżkę według reguł poniżej. W podsumowaniu podaj obie: co rekomendował CLM i co wybrałeś. Jeśli narzędzie zwróci błąd, po prostu idź dalej.

## 1. Czy w ogóle potrzebny jest Claude Code

Nie wołaj Claude Code, jeśli zadanie nie wymaga zmiany kodu w repozytorium: streszczenie, raport, lista, zebranie informacji — zrób to sam swoimi narzędziami.

## 2. Wybór modelu (pierwsza próba)

- **haiku** (`claude_code_run`, `model: haiku`) — zmiana mechaniczna i lokalna: literówka, wpis w konfiguracji, podbicie wersji, jedna funkcja z jasnym opisem.
- **sonnet** (`claude_code_run`, `model: sonnet`) — domyślnie: typowa poprawka błędu, nowy test, mała funkcja w kilku plikach.
- **opus** (`claude_code_opus`) — tylko gdy zadanie jest duże albo ryzykowne (jądro wykonania, exactly-once, migracje, bezpieczeństwo), albo gdy próba na sonnecie się nie udała. Zawsze czeka na zgodę człowieka — w zleceniu napisz, dlaczego Opus.

`effort`: `low` dla haiku, `medium` domyślnie, `high` tylko dla opusa albo po porażce.

## 3. Brief

Przed zleceniem zbierz kontekst sam (pliki, konwencje, testy), żeby Claude Code nie płacił za rozpoznanie. Zlecenie zawiera: cel, pliki do zmiany, ograniczenia, i **jak poznać sukces** (który test ma przejść).

## 4. Dowód, nie deklaracja

Po każdym udanym zleceniu, które coś zmieniło, wywołaj `sprawdz` z gałęzią z wyniku (`galaz`) i sprawdzeniem pasującym do repozytorium (np. `kelter-testy`). Tylko `sprawdz` z `ok: true` oznacza sukces — słowa „gotowe” w wyniku Claude Code nie są dowodem. Jeśli wynik zlecenia ma `blad` zaczynający się od „naruszenie”, sesja użyła Opusa bez zgody — nie ponawiaj tego samego zlecenia, zakończ i opisz to w podsumowaniu.

## 5. Eskalacja

- `sprawdz` nie przeszło po haiku → jedna próba na sonnecie.
- Nie przeszło po sonnecie → `claude_code_opus` (zgoda człowieka), z opisem, co poszło nie tak, i wyjściem sprawdzenia.
- Nie przeszło po opusie → zakończ i opisz problem; nie ponawiaj w kółko.

Każda próba to nowa gałąź; nie zakładaj, że poprzednia gałąź zawiera zmiany następnej.

## 6. Zakończenie

W podsumowaniu podaj: gałąź z udanym sprawdzeniem, użyte modele, łączny `koszt_usd` (przy subskrypcji to koszt umowny — miara zużycia limitów). Nic nie jest wypychane — człowiek pobiera gałąź sam.
