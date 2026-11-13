/* net.h's API: one client (client.c) over one WebSocket (ws.h). The server's address
   comes from net_init or the DHD_NET_SERVER environment variable (the official server unless
   dhd.ini or the environment names another, port/hle/host.c). */
#include "net.h"

#include <ctype.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include <SDL3/SDL_timer.h>

#include "client.h"
#include "json.h"
#include "ws.h"

_Static_assert(sizeof(NetLanRoom) == 136, "Rust LAN discovery result size");
_Static_assert(offsetof(NetLanRoom, max_players) == 128, "Rust LAN discovery capacity offset");
_Static_assert(offsetof(NetLanRoom, rtt_ms) == 132, "Rust LAN discovery latency offset");

extern int lan_host_start(char *url_out, size_t url_cap, char *address_out, size_t address_cap);
extern int lan_guest_url(const char *address, char *url_out, size_t url_cap);
extern int lan_host_start_tailnet(const char *address, char *url_out, size_t url_cap, char *address_out, size_t address_cap);
extern int lan_tailnet_guest_url(const char *address, char *url_out, size_t url_cap);
extern int lan_discover_start(void);
extern int lan_discover_poll(NetLanRoom *out, int max);
extern int lan_interfaces(NetLanInterface *out, int cap);
extern int lan_host_select_ip(const char *address);
extern int lan_host_refresh_address(char *out, size_t cap);
extern void lan_host_stop(void);

enum { REJOIN_ATTEMPTS = 5, LAN_REJOIN_ATTEMPTS = 8, SFU_CODE_LENGTH = 6, P2P_CODE_LENGTH = 7 };

/* The rooms offered (host_api.h NET_OFFER_*). SFU rooms are not offered on phones yet (finding
   143): Android builds refuse every SFU room here. Online P2P rooms (findings 180, 186) still use
   the room service; LAN and Tailnet rooms use the host's in-process room service and local frame
   relay. Tailnet is a separate manually-addressed mode, not LAN discovery over UDP. */
#ifdef __ANDROID__
#define NET_OFFERED (NET_OFFER_P2P | NET_OFFER_LAN | NET_OFFER_TAILNET)
#elif defined(__EMSCRIPTEN__)
#define NET_OFFERED NET_OFFER_P2P
#else
#define NET_OFFERED (NET_OFFER_SFU | NET_OFFER_P2P | NET_OFFER_LAN | NET_OFFER_TAILNET)
#endif
#define NET_NOT_OFFERED "this kind of room is not offered here yet"

static NetClient client;
static WsConn *conn;
static int inited;
static char base[256], name[100], version[64], iso[65], next_url[300], lan_address[64], lan_url[128];
static SDL_Mutex *api;  /* serialises connect, leave and the reset between rooms */
static double lan_address_refresh_ms;
static int tailnet_room;

static void send_text(void *ws, const char *text) { ws_send(ws, 0, text, strlen(text)); }
static void send_binary(void *ws, const void *data, size_t n) { ws_send(ws, 1, data, n); }

static void on_open(void *ctx, WsConn *ws)
{
    NetTransport tr;
    (void)ctx;
    tr.ctx = ws, tr.send_text = send_text, tr.send_binary = send_binary;
    client_connected(&client, &tr);
}

static void on_message(void *ctx, int binary, const uint8_t *data, size_t n)
{
    (void)ctx;
    if (binary)
        client_on_binary(&client, data, n);
    else
        client_on_text(&client, (const char *)data, n);
}

/* If a phone's Wi-Fi address changed during a drop, find this exact host session again rather than
   blindly retrying the stale address. Discovery is local-only, bounded to its existing 1.5 s scan,
   and its packets contain no PIN. The old endpoint remains a fallback when no room is found. */
static void rediscover_lan_room(const char *room_id)
{
    NetLanRoom rooms[16];
    double deadline;
    if (!room_id || strlen(room_id) != 16 || !net_discover_lan_start()) return;
    deadline = net_now_ms() + 2500.0;
    while (net_now_ms() < deadline) {
        int n = net_discover_lan_poll(rooms, 16), i;
        if (n >= 0) {
            for (i = 0; i < n; ++i) {
                char url[128];
                if (strcmp(rooms[i].room_id, room_id) != 0) continue;
                if (!lan_guest_url(rooms[i].address, url, sizeof url)) return;
                snprintf(lan_address, sizeof lan_address, "%s", rooms[i].address);
                snprintf(lan_url, sizeof lan_url, "%s", url);
                return;
            }
            return; /* scan completed; keep the previous endpoint if this host was not found */
        }
        if (n != -2) return;
        SDL_Delay(50);
    }
}

static const char *on_close(void *ctx, int code, const char *reason, int attempt)
{
    NetLobby lobby;
    (void)ctx;
    if (!client_on_closed(&client, code, reason))
        return NULL;
    if (client.lobby.is_lan) {
        client_lobby(&client, &lobby);
        if (attempt >= LAN_REJOIN_ATTEMPTS || lobby.reconnect_remaining_ms <= 0 || !lan_url[0]) {
            client_rejoin_failed(&client);
            return NULL;
        }
        if (!tailnet_room) rediscover_lan_room(lobby.lan_room_id);
        return lan_url;
    }
    if (attempt >= REJOIN_ATTEMPTS) {
        client_rejoin_failed(&client);
        return NULL;
    }
    client_lobby(&client, &lobby);
    snprintf(next_url, sizeof next_url, "%s/v1/rooms/%s", base, lobby.code);
    return next_url;
}

static int on_tick(void *ctx)
{
    /* the send thread of conn itself: net_leave clears conn before it stops the connection, and
       a connection's threads end before the next one starts */
    WsConn *ws = conn;
    (void)ctx;
    client_race_rtt(&client, ws ? ws_race_rtt_ms(ws) : -1);
    return client_tick(&client);
}

void net_init(const char *base_url, const char *player_name, const char *build_version, const char *iso_sha256)
{
    const char *url = base_url && *base_url ? base_url : getenv("DHD_NET_SERVER");
    size_t n;
    if (!inited) {
        api = SDL_CreateMutex();
        client_init(&client, "", "", "");
        inited = 1;
    }
    snprintf(base, sizeof base, "%s", url ? url : "");
    n = strlen(base);
    while (n && base[n - 1] == '/')
        base[--n] = 0;
    snprintf(name, sizeof name, "%s", player_name);
    snprintf(version, sizeof version, "%s", build_version);
    snprintf(iso, sizeof iso, "%s", iso_sha256);
}

static int connect_url(const char *url, int p2p, int lan, int leave_first, const char *lan_pin)
{
    WsHandler h = {NULL, on_open, on_message, on_close, on_tick};
    if (!inited)
        net_init(NULL, "Rider", "", "");
    if (leave_first)
        net_leave();
    SDL_LockMutex(api);
    client_free(&client);
    client_init(&client, name, version, iso);
    /* the install id (port/hle/host.c; finding 161), read here on the game's thread, the one that
       sets the environment */
    client_set_install_id(&client, getenv("DHD_NET_INSTALL_ID"));
    if (lan_pin && !client_set_lan_pin(&client, lan_pin)) {
        client.lobby.state = NET_FAILED;
        snprintf(client.lobby.error_code, sizeof client.lobby.error_code, "lan_pin");
        snprintf(client.lobby.error, sizeof client.lobby.error, "Enter a PIN of 4-8 digits, or leave it blank.");
        SDL_UnlockMutex(api);
        return -1;
    }
    if (!lan && !(NET_OFFERED & (p2p ? NET_OFFER_P2P : NET_OFFER_SFU))) {
        client.lobby.state = NET_FAILED;
        snprintf(client.lobby.error, sizeof client.lobby.error, NET_NOT_OFFERED);
        SDL_UnlockMutex(api);
        return -1;
    }
    if (!lan && !base[0]) {
        client.lobby.state = NET_FAILED;
        snprintf(client.lobby.error, sizeof client.lobby.error, "no server: set DHD_NET_SERVER");
        SDL_UnlockMutex(api);
        return -1;
    }
    client.lobby.state = NET_CONNECTING;
    client.lobby.is_p2p = p2p;  /* until the welcome says (client.c) */
    client.lobby.is_lan = lan;
    snprintf(client.lobby.lan_address, sizeof client.lobby.lan_address, "%s", lan ? lan_address : "");
    conn = ws_start(url, &h);
    SDL_UnlockMutex(api);
    return 0;
}

static int connect_to(const char *path, int p2p)
{
    char url[300];
    if (!inited)
        net_init(NULL, "Rider", "", "");
    snprintf(url, sizeof url, "%s%s", base, path);
    return connect_url(url, p2p, 0, 1, NULL);
}

int net_create_room(int p2p)
{
    /* the region its creator picked (finding 181): a hint's letters only, never anything else in
       the URL; "auto", unset or anything odd lets the server pick. A P2P room (finding 186) is a
       room like any other, with another race transport: its rules and its members' SFU fallback
       run there too. */
    const char *region = getenv("DHD_NET_ROOM_REGION");
    char path[80];
    int n = snprintf(path, sizeof path, "/v1/rooms/new%s", p2p ? "?transport=p2p" : "");
    if (region && *region && strcmp(region, "auto") && strlen(region) < 16 && strspn(region, "abcdefghijklmnopqrstuvwxyz-") == strlen(region))
        snprintf(path + n, sizeof path - n, "%cregion=%s", p2p ? '&' : '?', region);
    return connect_to(path, p2p);
}

int net_join_room(const char *code)
{
    static const char alphabet[] = "23456789ABCDEFGHJKMNPQRSTUVWXYZ";
    char path[32], up[P2P_CODE_LENGTH + 2];
    int i, p2p;
    for (i = 0; code[i] && i < P2P_CODE_LENGTH + 1; i++)
        up[i] = (char)toupper((unsigned char)code[i]);
    up[i] = 0;
    p2p = i == P2P_CODE_LENGTH;
    if ((i != SFU_CODE_LENGTH && !p2p) || strspn(up, alphabet) != (size_t)i)
        return -1;
    if (!(NET_OFFERED & (p2p ? NET_OFFER_P2P : NET_OFFER_SFU)))
        return -1;
    snprintf(path, sizeof path, "/v1/rooms/%s", up);
    return connect_to(path, p2p);
}

static void failed_connect(const char *message)
{
    if (!inited) net_init(NULL, "Rider", "", "");
    net_leave();
    SDL_LockMutex(api);
    client_free(&client);
    client_init(&client, name, version, iso);
    client.lobby.state = NET_FAILED;
    snprintf(client.lobby.error_code, sizeof client.lobby.error_code, "lan_setup");
    snprintf(client.lobby.error, sizeof client.lobby.error, "%s", message);
    SDL_UnlockMutex(api);
}

int net_host_lan(void)
{
    char url[128], address[64];
    if (!inited) net_init(NULL, "Rider", "", "");
    net_leave();
    lan_address[0] = 0;
    lan_address_refresh_ms = 0;
    if (!lan_host_start(url, sizeof url, address, sizeof address)) {
        failed_connect("Connect to Wi-Fi before hosting a LAN room.");
        return -1;
    }
    snprintf(lan_address, sizeof lan_address, "%s", address);
    snprintf(lan_url, sizeof lan_url, "%s", url);
    return connect_url(url, 0, 1, 0, NULL);
}

int net_host_tailnet(const char *address)
{
    char url[128], endpoint[64];
    if (!inited) net_init(NULL, "Rider", "", "");
    net_leave();
    lan_address[0] = 0;
    lan_address_refresh_ms = 0;
    if (!lan_host_start_tailnet(address, url, sizeof url, endpoint, sizeof endpoint)) {
        failed_connect("Enter this device's Tailscale IPv4 address (100.64-127.x.x) before hosting.");
        return -1;
    }
    tailnet_room = 1;
    snprintf(lan_address, sizeof lan_address, "%s", endpoint);
    snprintf(lan_url, sizeof lan_url, "%s", url);
    return connect_url(url, 0, 1, 0, NULL);
}

int net_join_lan(const char *address)
{
    return net_join_lan_pin(address, "");
}

int net_join_lan_pin(const char *address, const char *pin)
{
    char url[128];
    if (!inited) net_init(NULL, "Rider", "", "");
    if (!pin) pin = "";
    size_t pin_len = strlen(pin);
    if (pin_len && (pin_len < 4 || pin_len > 8 || strspn(pin, "0123456789") != pin_len)) {
        failed_connect("Enter a PIN of 4-8 digits, or leave it blank.");
        return -1;
    }
    if (!lan_guest_url(address, url, sizeof url)) {
        failed_connect("Enter the host's private IPv4 address and port.");
        return -1;
    }
    net_leave();
    snprintf(lan_address, sizeof lan_address, "%s", address);
    snprintf(lan_url, sizeof lan_url, "%s", url);
    return connect_url(url, 0, 1, 0, pin);
}

int net_join_tailnet(const char *address)
{
    return net_join_tailnet_pin(address, "");
}

int net_join_tailnet_pin(const char *address, const char *pin)
{
    char url[128];
    if (!inited) net_init(NULL, "Rider", "", "");
    if (!pin) pin = "";
    size_t pin_len = strlen(pin);
    if (pin_len && (pin_len < 4 || pin_len > 8 || strspn(pin, "0123456789") != pin_len)) {
        failed_connect("Enter a PIN of 4-8 digits, or leave it blank.");
        return -1;
    }
    if (!lan_tailnet_guest_url(address, url, sizeof url)) {
        failed_connect("Enter the host's Tailscale 100.x IPv4 address and port.");
        return -1;
    }
    net_leave();
    tailnet_room = 1;
    snprintf(lan_address, sizeof lan_address, "%s", address);
    snprintf(lan_url, sizeof lan_url, "%s", url);
    return connect_url(url, 0, 1, 0, pin);
}

int net_discover_lan_start(void) { return lan_discover_start(); }
int net_discover_lan_poll(NetLanRoom *out, int max)
{
    if (max < 0 || max > 16 || (max && !out)) return -1;
    return lan_discover_poll(out, max);
}
int net_lan_set_pin(const char *pin) { return inited && client_set_room_pin(&client, pin); }
int net_lan_interfaces(NetLanInterface *out, int max)
{
    if (max < 0 || max > 16 || (max && !out)) return -1;
    return lan_interfaces(out, max);
}
int net_lan_select_ip(const char *address)
{
    if (!address || !lan_host_select_ip(address)) return 0;
    return net_lan_refresh_address();
}
int net_lan_refresh_address(void)
{
    char address[64];
    if (!lan_host_refresh_address(address, sizeof address)) {
        lan_address[0] = 0;
        return 0;
    }
    snprintf(lan_address, sizeof lan_address, "%s", address);
    return 1;
}
void net_lan_check_connections(void) { if (inited) client_check_connections(&client); }

void net_leave(void)
{
    WsConn *c;
    if (!inited)
        return;
    SDL_LockMutex(api);
    c = conn, conn = NULL;
    if (c) {
        client_leave(&client);
        ws_stop(c);
    }
    lan_host_stop();
    lan_address[0] = 0;
    lan_url[0] = 0;
    tailnet_room = 0;
    SDL_UnlockMutex(api);
}

int net_list_lobbies(NetLobbyInfo *out, int max)
{
    static char body[64 * 1024];
    char url[300];
    double sent, got;
    int n;
    if (!(NET_OFFERED & NET_OFFER_SFU) || !base[0] || !inited)  /* P2P rooms are never listed */
        return -1;
    snprintf(url, sizeof url, "%s/v1/lobbies", base);
    sent = net_now_ms();
    if (http_get(url, body, sizeof body) < 0)
        return -1;
    got = net_now_ms();
    SDL_LockMutex(api);  /* not while connect_to makes the client again */
    n = client_read_lobbies(&client, body, sent, got, out, max);
    SDL_UnlockMutex(api);
    return n;
}

void net_set_room(const NetRoomSettings *s) { client_set_room(&client, s); }
void net_pick(int member, int rider, int bike) { client_pick(&client, member, rider, bike); }
void net_set_ready(int ready) { client_ready(&client, ready); }
void net_kick(int member) { client_kick(&client, member); }
void net_start_race(void) { client_start(&client); }
void net_end_race(void) { client_end(&client); }
void net_finish(unsigned ticks, int place) { client_finish(&client, ticks, place); }
void net_back(void) { client_back(&client); }
void net_rules(int finish_window, int next_race, int then) { client_rules(&client, finish_window, next_race, then); }
void net_rotation(const unsigned char *ids, int count) { client_rotation(&client, ids, count); }
void net_race_go(double at) { client_race_go(&client, at); }

void net_get_lobby(NetLobby *out)
{
    if (!inited) {
        memset(out, 0, sizeof *out);
        return;
    }
    client_lobby(&client, out);
    out->is_tailnet = out->is_lan && tailnet_room;
    if (out->is_lan && out->is_host && !tailnet_room) {
        double now = net_now_ms();
        if (now - lan_address_refresh_ms >= 1000.0) {
            lan_address_refresh_ms = now;
            net_lan_refresh_address();
        }
    }
    if (out->is_lan && lan_address[0])
        snprintf(out->lan_address, sizeof out->lan_address, "%s", lan_address);
    else if (out->is_lan)
        out->lan_address[0] = 0;
}

double net_seconds(void)
{
    return net_now_ms() / 1000.0;
}

double net_server_ms(void)
{
    return net_now_ms() + client.clock_offset;
}

/* This PC's ping while in a room, else -1 (the FPS & Ping counter, port/gs/gpu_rust.c): a few
   fields, read as net_racing reads its flag, not a copy of the lobby for every picture. */
int net_ping_ms(void)
{
    const NetLobby *l = &client.lobby;
    return inited && (l->state == NET_LOBBY || l->state == NET_RACING) && !l->offline && l->code[0] ? l->ping_ms : -1;
}

/* In a race, the other riders' delay (ms; client_riders_ms), else -1: the counter shows it
   instead of the ping to the room's server, which carries no race data (finding 169). */
int net_race_ms(void)
{
    const NetLobby *l = &client.lobby;
    return inited && (l->state == NET_LOBBY || l->state == NET_RACING) && !l->offline && l->code[0] ? client_race_ms(&client) : -1;
}

int net_riders_ms(void)
{
    return inited && client.racing && !client.lobby.offline ? client_riders_ms(&client) : -1;
}

int net_racing(void)
{
    return inited && client.racing;
}

int net_local_slot(void)
{
    return client.lobby.local_slot;
}





int net_send_frame(int type, const void *payload, size_t n)
{
    return inited && client_send_frame(&client, type, payload, n);
}

int net_receive_frame(uint8_t *out, size_t max)
{
    return inited ? client_receive_frame(&client, out, max) : 0;
}


/* Guest-address-only module boundary; native device code stays above. */
#include "../host/host_api.h"
#include "../include/guest.h"
void host_service_net_init(uint32_t a0, uint32_t a1, uint32_t a2, uint32_t a3) { net_init((const char *)GUEST(a0), (const char *)GUEST(a1), (const char *)GUEST(a2), (const char *)GUEST(a3)); }
int host_service_net_offered(void) { return NET_OFFERED; }
int host_service_net_create_room(int p2p) { return net_create_room(p2p); }
int host_service_net_join_room(uint32_t a0) { return net_join_room((const char *)GUEST(a0)); }
int host_service_net_host_lan(void) { return net_host_lan(); }
int host_service_net_host_tailnet(uint32_t a0) { return net_host_tailnet((const char *)GUEST(a0)); }
int host_service_net_join_lan(uint32_t a0) { return net_join_lan((const char *)GUEST(a0)); }
int host_service_net_join_lan_pin(uint32_t a0, uint32_t a1) { return net_join_lan_pin((const char *)GUEST(a0), (const char *)GUEST(a1)); }
int host_service_net_join_tailnet_pin(uint32_t a0, uint32_t a1) { return net_join_tailnet_pin((const char *)GUEST(a0), (const char *)GUEST(a1)); }
int host_service_net_set_lan_pin(uint32_t a0) { return net_lan_set_pin((const char *)GUEST(a0)); }
int host_service_net_discover_lan_start(void) { return net_discover_lan_start(); }
int host_service_net_discover_lan_poll(uint32_t a0, int max) { return net_discover_lan_poll((NetLanRoom *)GUEST(a0), max); }
int host_service_net_lan_interfaces(uint32_t a0, int max) { return net_lan_interfaces((NetLanInterface *)GUEST(a0), max); }
int host_service_net_lan_select_ip(uint32_t a0) { return net_lan_select_ip((const char *)GUEST(a0)); }
int host_service_net_lan_refresh_address(void) { return net_lan_refresh_address(); }
void host_service_net_lan_check(void) { net_lan_check_connections(); }
void host_service_net_leave(void) { net_leave(); }
void host_service_net_get_lobby(uint32_t a0) { net_get_lobby((NetLobby *)GUEST(a0)); }
void host_service_net_pick(int a0, int a1, int a2) { net_pick(a0, a1, a2); }
void host_service_net_finish(uint32_t ticks, int place) { net_finish(ticks, place); }
void host_service_net_back(void) { net_back(); }
void host_service_net_rules(int finish_window, int next_race, int then) { net_rules(finish_window, next_race, then); }
void host_service_net_rotation(uint32_t address, int count) { if (count >= 0 && count <= 64) net_rotation((const unsigned char *)GUEST(address), count); }
void host_service_net_end_race(void) { net_end_race(); }
void host_service_net_race_go(double at) { net_race_go(at); }
void host_service_net_set_ready(int a0) { net_set_ready(a0); }
void host_service_net_kick(int member) { net_kick(member); }
void host_service_net_start_race(void) { net_start_race(); }
int host_service_net_racing(void) { return net_racing(); }
double host_service_net_seconds(void) { return net_seconds(); }
double host_service_net_server_ms(void) { return net_server_ms(); }
int host_service_net_send_frame(int a0, uint32_t a1, uint32_t a2) { return net_send_frame(a0, (const void *)GUEST(a1), a2); }
int host_service_net_receive_frame(uint32_t a0, uint32_t a1) { return net_receive_frame((uint8_t *)GUEST(a0), a1); }
int host_service_net_list_lobbies(uint32_t a0, int a1) { return net_list_lobbies((NetLobbyInfo *)GUEST(a0), a1); }

void host_service_net_set_room(uint32_t address) {
    const uint32_t *in=GUEST(address);
    NetRoomSettings out;
    out.track=(int)in[0]; out.laps=(int)in[1];
    out.is_public=(int)in[2]; out.max_players=(int)in[3];
    memcpy(&out.seed,in+4,8);
    out.room_name=in[6] ? GUEST(in[6]) : NULL;  /* 0: the name stays (a track or bots change) */
    net_set_room(&out);
}
