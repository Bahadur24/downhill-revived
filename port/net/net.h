/* Online races: the API the rest of the port uses (docs/ONLINE-PLAN.md, finding 039, the wire
   protocol in docs/online-protocol.md).

   Races use state sync (findings 041, 044): every PC runs its own one-player race and sends
   snapshots of the riders it owns (STATE frames) and hits it deals (EVENT frames) with
   net_send_frame; online rooms use the SFU/P2P transport and LAN rooms use the host's local relay.
   The others' riders are puppets (port/hle/online.c, port/hle/puppet.c). Background threads keep
   the room's WebSocket (lobby JSON, binary pings) and any online SFU association. */
#ifndef NET_H
#define NET_H

#include <stddef.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>

enum { NET_MAX_SLOTS = 10 };  /* online rider slots, independent of local controllers */

/* ---- connection and lobby ---- */

enum { NET_IDLE, NET_CONNECTING, NET_LOBBY, NET_RACING, NET_RECONNECTING, NET_FAILED };

/* Shared core/host ABI: fixed-width ints and doubles, no pointers. Append fields only. */
typedef struct {
    int id;                    /* member id: fixed for the connection */
    char name[100];            /* UTF-8, up to 24 characters */
    int ready, rider, bike;
    int rtt_ms;                /* the server's measure, -1 unknown */
    int choosing;              /* on the rider screen */
    int in_race;
    char race_state[12], dnf_reason[16];
    long long finish_ticks;     /* -1 when absent */
    int place;
    char region[8];            /* their region as Cloudflare places them ("EU"...), "" unknown;
                                  never their country (finding 135) */
    /* LAN connectivity check; appended for core/host ABI compatibility. */
    uint64_t check_id;
    int check_state;           /* 0 none, 1 pending, 2 reachable, 3 timed out */
    int connection_state;      /* 0 connected, 1 reconnecting */
    int reconnect_ms;          /* server-reported remaining reconnect grace, else 0 */
} NetMember;

typedef struct {
    int slot, member_id;
    char name[100];
    int rider, bike;
    int left;                  /* gone: the host's AI drives the rider */
} NetRacer;

typedef struct {
    int id, rider, place;
    char name[100], dnf_reason[16];
    long long ticks;            /* -1 for DNF */
} NetResult;

typedef struct {
    int state;                 /* NET_IDLE... */
    unsigned seq;              /* bumps on every change */
    char code[8];              /* the join code */
    int member_id, host_id, is_host;
    char room_name[100];
    int is_public, max_players, started;
    int is_p2p;                /* a P2P room (findings 180, 186): races over a star of games; never public */
    int track, laps;
    long long seed;            /* -1 until set */
    int nmembers;
    NetMember member[NET_MAX_SLOTS];
    int ping_ms;               /* this PC to the relay, -1 unknown */
    /* the race, once started */
    int local_slot, nslots;
    NetRacer racer[NET_MAX_SLOTS];
    int race_track, race_laps;
    uint32_t race_seed;
    double start_at;           /* net_seconds() of the server's start */
    int offline;               /* the connection dropped mid-race */
    /* the last error */
    char error_code[32], error[200];
    /* Everyone is ready: the race starts by itself in this many ms (the server's countdown),
       -1 when none runs. Worked out as the copy is made (net_get_lobby). */
    int countdown_ms;
    char phase[16], last_event[32];
    /* Server epoch ms; zero means absent. Compare with net_server_ms(). */
    double race_started_at, finish_ends_at, next_race_at, countdown_ends_at, server_time;
    int waiting_for_rider, rules_pending;
    int finish_window, next_race, then; /* next: 0 auto/1 host; then: 0 same/1 next/2 random */
    int rotation_count;
    unsigned char rotation[64];
    int last_race_id, last_race_track, nresults;
    NetResult result[NET_MAX_SLOTS];
    /* Where this PC and the room are (finding 135), "" unknown or an older server: the Cloudflare
       location this connection reached, this PC's country and continent as Cloudflare sees them,
       the location the room runs in and its region ("EU", "NA", "ASIA"...). */
    char edge_colo[8], country[4], continent[4], room_colo[8], room_region[8];
    /* Where the room runs, finer ("EU WEST", "ASIA SE"...; "" unknown or an older server), and
       whether its creator picked the region (finding 181). */
    char room_area[12];
    int room_chosen;
    /* The server's notice before it restarts the room for new code (finding 184): a footer line
       while this PC rejoins ("Server updated, reconnecting..."); "" none. The welcome clears it. */
    char notice[64];
    /* The race's start moment as the room sent it (server ms; 0 none yet): the host's START over
       the room as well as the race link, for a game whose race link brought nothing (finding 187). */
    double race_go_at;
    /* Appended to preserve the shared core/host ABI: local room metadata, no online service. */
    int is_lan;
    char lan_address[64];       /* host's private IPv4:port, shown to guests for manual entry */
    /* The latest LAN pre-race check; appended for ABI compatibility. */
    uint64_t check_id;
    double check_deadline;
    int check_valid;
    int pin_required;           /* LAN room only; never contains the PIN */
    int reconnect_remaining_ms; /* this PC's remaining LAN reconnect grace; 0 otherwise */
    int reconnect_attempt;      /* number of reconnect attempts made */
    char lan_room_id[17];       /* stable per-host-session id for LAN rediscovery */
    int is_tailnet;              /* LAN room reached through Tailscale; appended ABI field */
} NetLobby;

/* Host settings; fields at -1 (room_name NULL) are left as they are. */
typedef struct {
    int track, laps, is_public, max_players;
    long long seed;
    const char *room_name;
} NetRoomSettings;

typedef struct {
    char code[8], room_name[100], version[64], host_name[100];
    int track, players, max_players;
    char phase[16];
    double race_started_at, finish_ends_at, next_race_at;
    int full;
    char region[8], colo[8];   /* where the room runs ("EU"...; finding 135), "" unknown */
    int timed;                 /* the times above can be counted against net_server_ms(): the list
                                  came with the server's "now", or a room's pings set the clock */
    char area[12];             /* where the room runs, finer ("EU WEST"...; finding 181), "" unknown */
} NetLobbyInfo;

/* A discovered private LAN room. The address is only used locally and never sent online. */
typedef struct {
    char address[64];
    char room_name[32];
    int players, track;
    char room_id[17];
    int pin_required;
    int max_players;
    int rtt_ms;                 /* measured by this phone's current discovery scan; -1 unknown */
} NetLanRoom;

/* One currently assigned, bindable local private IPv4 interface. */
typedef struct {
    char name[16];
    char address[16];
    int is_wifi;
    int selected;
} NetLanInterface;

/* base_url: "wss://host" or "ws://127.0.0.1:8787". version: the build's version string (the
   server refuses players whose version differs from the host's). iso_sha256: 64 lowercase hex. */
void net_init(const char *base_url, const char *name, const char *version, const char *iso_sha256);
/* host_service_net_offered's bits: the kinds of room this build offers (port/host/host_api.h). */
enum { NET_OFFER_SFU = 1, NET_OFFER_P2P = 2, NET_OFFER_LAN = 4, NET_OFFER_TAILNET = 8 };

/* A room code has 6 characters (an SFU room) or 7 (a P2P room, findings 180, 186): both are
   server rooms, a P2P one racing over a star of the games' own connections. A room runs where
   DHD_NET_ROOM_REGION says (dhd.ini room_region, the ONLINE page's Room Region row): a Durable
   Object location hint the server offers ("weur", "apac-se"...), or "auto"/unset: the server picks
   (finding 181). */
int net_create_room(int p2p);
int net_join_room(const char *code);   /* -1 when the code can't be one */
int net_host_lan(void);                /* starts the local host service and joins its own room */
int net_host_tailnet(const char *address); /* private local room, advertised at this device's Tailscale IPv4 */
int net_join_lan(const char *address); /* connects to an IPv4:port entered from the LAN menu */
int net_join_lan_pin(const char *address, const char *pin); /* LAN join with optional PIN */
int net_join_tailnet(const char *address); /* connects only to a Tailscale 100.64.0.0/10 IPv4:port */
int net_join_tailnet_pin(const char *address, const char *pin); /* Tailscale join with optional room PIN */
int net_lan_set_pin(const char *pin);  /* host-only; empty clears the room PIN */
int net_discover_lan_start(void);      /* starts an asynchronous bounded UDP scan */
int net_discover_lan_poll(NetLanRoom *out, int max); /* -2 working, -1 failed, else count */
int net_lan_interfaces(NetLanInterface *out, int max); /* actual local private IPv4 interfaces */
int net_lan_select_ip(const char *address);            /* host-only, assigned local address only */
int net_lan_refresh_address(void);                     /* refreshes current LAN advertisement */
void net_lan_check_connections(void);  /* host-only fresh LAN connectivity check */

/* A room's code as dhd.log shows it: net_log_code(code).text. Anyone holding an open P2P room's
   code can join it and so learn its hub's network address, and players post dhd.log in public:
   a P2P room's code (7 characters) shows only its first two ("AB*****"; finding 180). An SFU
   room's (6) shows as it is. DHD_NET_LOG_CODE=1 shows every code in full: the test tools read the
   host's code from the log (docs/TESTING.md). Core and host both use it, so it lives here. */
typedef struct { char text[8]; } NetLogCode;
static inline NetLogCode net_log_code(const char *code)
{
    NetLogCode c;
    size_t n = code ? strlen(code) : 0;
    memset(&c, 0, sizeof c);
    if (n > sizeof c.text - 1)
        n = sizeof c.text - 1;
    memcpy(c.text, code, n);
    if (n == 7 && !getenv("DHD_NET_LOG_CODE"))
        memset(c.text + 2, '*', 5);
    return c;
}
void net_leave(void);
/* Public lobbies (blocking HTTP GET); returns how many, -1 on error. */
int net_list_lobbies(NetLobbyInfo *out, int max);
void net_set_room(const NetRoomSettings *s);       /* host */
/* member -1: this PC. rider -1 (this PC): choosing a rider now, unready until the next pick. */
void net_pick(int member, int rider, int bike);
void net_set_ready(int ready);
void net_kick(int member);                         /* host */
void net_start_race(void);                         /* host; session: ready entrants */
void net_end_race(void);                           /* host: back to the lobby */
/* Session room calls. next_race: 0 auto, 1 host; then: 0 same, 1 next, 2 random. */
void net_finish(unsigned ticks, int place);
void net_back(void);
void net_rules(int finish_window, int next_race, int then);
void net_rotation(const unsigned char *ids, int count);
/* The race's start moment (server ms) this game named (finding 187): to the room too, which sends
   it to the racers whose race link may have lost the START event. Only a server that lists
   "race_go" in its welcome hears it. */
void net_race_go(double at);
void net_get_lobby(NetLobby *out);                 /* a copy, for the UI */
double net_seconds(void);                          /* the clock start_at uses */
/* The server's clock (ms since the Unix epoch), estimated from the lobby pings' round-trips: the
   same moment on every PC to a few ms, for starting the race together. */
double net_server_ms(void);
int net_ping_ms(void);                             /* in a room, else -1 */
/* In a room: the round trip to Cloudflare's race network (the room's SFU association; the median
   of the last 5 one-second samples), else -1 (finding 181). */
int net_race_ms(void);
/* In a race: how old the other riders' snapshots are when they arrive (the slowest player's
   median, ms; what the puppets are predicted across), else -1 (finding 169). */
int net_riders_ms(void);

/* ---- the race ---- */

int net_racing(void);
int net_local_slot(void);

/* Race frames: online SFU/P2P rooms carry them over their race transport; LAN rooms relay them over
   the host's local WebSocket. A STATE bundle holds riders this PC owns; an EVENT is one it decided
   for another PC's rider. A frame is type, slot (the receiver sets it from the channel), payload. */
/* NET_FRAME_MAX: the largest frame accepted. A STATE frame is one bundle of at most
   STATE_BUNDLE_MAX bytes (port/hle/state_bundle.h: as many riders as fit one datagram, finding
   169); a PC with more riders sends several. */
enum { NET_FRAME_STATE = 5, NET_FRAME_EVENT = 6, NET_FRAME_MAX = 3072, NET_RELAY_QUEUE = 128 };
/* The race's pings over the room WebSocket, its only binary messages (client.c): type, slot, the
   send time; the server answers PONG with the same bytes and its clock. */
enum { NET_FRAME_PING = 3, NET_FRAME_PONG = 4 };
/* Sends type and payload (at most NET_FRAME_MAX - 2 bytes); 0 when not racing. */
int net_send_frame(int type, const void *payload, size_t n);
/* The oldest frame received, whole (out[0] type, out[1] sender's slot); its size, 0 if none. */
int net_receive_frame(uint8_t *out, size_t max);

/* ---- state hashes ---- */

typedef struct {
    const void *base;
    size_t size;
} NetRange;

/* A 64-bit hash (XXH64 per range, chained through the seed) of memory ranges: online.c's
   DHD_HASH_LOG of the race state, every NET_HASH_PERIOD fields. */
enum { NET_HASH_PERIOD = 60 };
uint64_t net_hash_ranges(const NetRange *ranges, int n);
uint64_t net_hash64(const void *data, size_t size, uint64_t seed);  /* XXH64 */

#endif
