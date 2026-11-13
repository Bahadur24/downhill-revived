/* Local-only race history persisted in the player's dhd.ini. This is an aggregate, not a server
   feature: it stores per-track finishes/placements and best time, plus the last room/race pair so a
   repeated lobby snapshot cannot count the same race twice. */
#ifndef LAN_HISTORY_H
#define LAN_HISTORY_H

#include <limits.h>
#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>

#define LAN_HISTORY_TRACKS 25
#define LAN_HISTORY_PLACES 10
#define LAN_HISTORY_ROOM_ID 16
#define LAN_HISTORY_TEXT_MAX 4096

typedef struct {
    uint32_t races;
    uint32_t place[LAN_HISTORY_PLACES]; /* index 0 is first place */
    uint64_t best_ticks;                 /* 60 ticks per second; 0 means no finish yet */
} LanHistoryTrack;

typedef struct {
    char room_id[LAN_HISTORY_ROOM_ID + 1];
    uint32_t last_race_id;
    LanHistoryTrack track[LAN_HISTORY_TRACKS];
} LanHistory;

static inline int lan_history_read_number(const char **at, char delimiter, uint64_t *out)
{
    uint64_t value = 0;
    const char *p = *at;
    if (*p < '0' || *p > '9') return 0;
    do {
        unsigned digit = (unsigned)(*p - '0');
        if (value > (UINT64_MAX - digit) / 10) return 0;
        value = value * 10 + digit;
        p++;
    } while (*p >= '0' && *p <= '9');
    if (*p != delimiter) return 0;
    *at = p + 1;
    *out = value;
    return 1;
}

static inline int lan_history_parse(const char *text, LanHistory *history)
{
    LanHistory parsed = {0};
    const char *p, *end;
    uint64_t value;
    size_t room_len;
    if (!text || !history || strncmp(text, "v1|", 3)) return 0;
    p = text + 3;
    end = strchr(p, '|');
    if (!end) return 0;
    room_len = (size_t)(end - p);
    if (room_len > LAN_HISTORY_ROOM_ID) return 0;
    memcpy(parsed.room_id, p, room_len);
    parsed.room_id[room_len] = 0;
    p = end + 1;
    if (!lan_history_read_number(&p, '|', &value) || value > UINT32_MAX) return 0;
    parsed.last_race_id = (uint32_t)value;
    for (int t = 0; t < LAN_HISTORY_TRACKS; t++) {
        if (!lan_history_read_number(&p, ',', &value) || value > UINT32_MAX) return 0;
        parsed.track[t].races = (uint32_t)value;
        for (int place = 0; place < LAN_HISTORY_PLACES; place++) {
            if (!lan_history_read_number(&p, ',', &value) || value > UINT32_MAX) return 0;
            parsed.track[t].place[place] = (uint32_t)value;
        }
        if (!lan_history_read_number(&p, ';', &parsed.track[t].best_ticks)) return 0;
    }
    if (*p) return 0;
    *history = parsed;
    return 1;
}

static inline int lan_history_format(const LanHistory *history, char *out, size_t cap)
{
    int n;
    size_t used;
    if (!history || !out || !cap) return 0;
    n = snprintf(out, cap, "v1|%.*s|%u|", LAN_HISTORY_ROOM_ID, history->room_id, history->last_race_id);
    if (n < 0 || (size_t)n >= cap) return 0;
    used = (size_t)n;
    for (int t = 0; t < LAN_HISTORY_TRACKS; t++) {
        const LanHistoryTrack *s = &history->track[t];
        n = snprintf(out + used, cap - used, "%u,", s->races);
        if (n < 0 || (size_t)n >= cap - used) return 0;
        used += (size_t)n;
        for (int place = 0; place < LAN_HISTORY_PLACES; place++) {
            n = snprintf(out + used, cap - used, "%u,", s->place[place]);
            if (n < 0 || (size_t)n >= cap - used) return 0;
            used += (size_t)n;
        }
        n = snprintf(out + used, cap - used, "%llu;", (unsigned long long)s->best_ticks);
        if (n < 0 || (size_t)n >= cap - used) return 0;
        used += (size_t)n;
    }
    return 1;
}

/* Returns 1 only when a new race was added. place is 1..10, or 0 for a DNF/unplaced result. */
static inline int lan_history_record(LanHistory *history, const char *room_id, uint32_t race_id,
                                     int track_index, int place, long long ticks)
{
    LanHistoryTrack *stats;
    size_t id_len;
    if (!history || !room_id || !race_id || track_index < 0 || track_index >= LAN_HISTORY_TRACKS) return 0;
    id_len = strlen(room_id);
    if (!id_len || id_len > LAN_HISTORY_ROOM_ID || strchr(room_id, '|')) return 0;
    if (!strcmp(history->room_id, room_id) && race_id <= history->last_race_id) return 0;
    snprintf(history->room_id, sizeof history->room_id, "%s", room_id);
    history->last_race_id = race_id;
    stats = &history->track[track_index];
    if (stats->races != UINT32_MAX) stats->races++;
    if (place >= 1 && place <= LAN_HISTORY_PLACES && stats->place[place - 1] != UINT32_MAX)
        stats->place[place - 1]++;
    if (ticks >= 0 && (!stats->best_ticks || (uint64_t)ticks < stats->best_ticks))
        stats->best_ticks = (uint64_t)ticks;
    return 1;
}

#endif
