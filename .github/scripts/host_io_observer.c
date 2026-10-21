/* macOS fixture instrumentation only. Never linked into Mynou. */
#include <arpa/inet.h>
#include <errno.h>
#include <fcntl.h>
#include <netdb.h>
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/socket.h>
#include <unistd.h>

static int log_fd = -1;
static pthread_mutex_t lock = PTHREAD_MUTEX_INITIALIZER;

static void record(const char *op, int fd, const struct sockaddr *address,
                   int family, int type) {
    int saved = errno;
    if (log_fd >= 0) {
        struct sockaddr_storage local;
        socklen_t length = sizeof(local);
        unsigned port = 0, source = 0;
        char ip[INET6_ADDRSTRLEN] = "";
        if (address && address->sa_family == AF_INET) {
            const struct sockaddr_in *a = (const struct sockaddr_in *)address;
            inet_ntop(AF_INET, &a->sin_addr, ip, sizeof(ip));
            port = ntohs(a->sin_port);
        } else if (address && address->sa_family == AF_INET6) {
            const struct sockaddr_in6 *a = (const struct sockaddr_in6 *)address;
            inet_ntop(AF_INET6, &a->sin6_addr, ip, sizeof(ip));
            port = ntohs(a->sin6_port);
        }
        if (fd >= 0 && getsockname(fd, (struct sockaddr *)&local, &length) == 0 &&
            local.ss_family == AF_INET)
            source = ntohs(((struct sockaddr_in *)&local)->sin_port);
        char line[512];
        int n = snprintf(line, sizeof(line),
            "{\"op\":\"%s\",\"pid\":%d,\"fd\":%d,\"family\":%d,"
            "\"type\":%d,\"source\":%u,\"ip\":\"%s\",\"port\":%u}\n",
            op, getpid(), fd, family, type, source, ip, port);
        pthread_mutex_lock(&lock);
        if (n <= 0 || (size_t)n >= sizeof(line) || write(log_fd, line, n) != n)
            _exit(125);
        pthread_mutex_unlock(&lock);
    }
    errno = saved;
}

__attribute__((constructor)) static void start(void) {
    const char *path = getenv("MYNOU_HOST_IO_TRACE");
    if (!path || !*path) _exit(125);
    log_fd = open(path, O_WRONLY | O_APPEND | O_CLOEXEC);
    if (log_fd < 0) _exit(125);
    record("loaded", -1, NULL, 0, 0);
}

static int observed_socket(int domain, int type, int protocol) {
    int fd = socket(domain, type, protocol);
    record("socket", fd, NULL, domain, type);
    return fd;
}
static int observed_bind(int fd, const struct sockaddr *address, socklen_t len) {
    record("bind", fd, address, address->sa_family, 0);
    int result = bind(fd, address, len);
    int saved = errno;
    if (result == 0) record("bound", fd, address, address->sa_family, 0);
    errno = saved;
    return result;
}
static int observed_connect(int fd, const struct sockaddr *address, socklen_t len) {
    record("connect", fd, address, address->sa_family, 0);
    return connect(fd, address, len);
}
static ssize_t observed_sendto(int fd, const void *data, size_t size, int flags,
                             const struct sockaddr *address, socklen_t len) {
    record("sendto", fd, address, address ? address->sa_family : 0, 0);
    return sendto(fd, data, size, flags, address, len);
}
static ssize_t observed_sendmsg(int fd, const struct msghdr *msg, int flags) {
    const struct sockaddr *address = msg->msg_name;
    record("sendmsg", fd, address, address ? address->sa_family : 0, 0);
    return sendmsg(fd, msg, flags);
}
static int observed_getaddrinfo(const char *node, const char *service,
                              const struct addrinfo *hints, struct addrinfo **out) {
    /* Intentionally omit names and payloads from the observation artifact. */
    record("getaddrinfo", -1, NULL, 0, 0);
    return getaddrinfo(node, service, hints, out);
}
static int observed_close(int fd) {
    int saved = errno;
    struct sockaddr_storage local;
    socklen_t length = sizeof(local);
    if (getsockname(fd, (struct sockaddr *)&local, &length) == 0 &&
        (local.ss_family == AF_INET || local.ss_family == AF_INET6))
        record("close", fd, NULL, 0, 0);
    errno = saved;
    return close(fd);
}

#define INTERPOSE(replacement, original) \
    __attribute__((used)) static const struct { \
        const void *replace; const void *with; \
    } pair_##original __attribute__((section("__DATA,__interpose"))) = { \
        (const void *)&replacement, (const void *)&original \
    }
INTERPOSE(observed_socket, socket);
INTERPOSE(observed_bind, bind);
INTERPOSE(observed_connect, connect);
INTERPOSE(observed_sendto, sendto);
INTERPOSE(observed_sendmsg, sendmsg);
INTERPOSE(observed_getaddrinfo, getaddrinfo);
INTERPOSE(observed_close, close);
