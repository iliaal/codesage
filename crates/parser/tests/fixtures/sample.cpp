#include <vector>
#include <string>
#include "local_header.h"

#define CPP_MAX 256

namespace app {
namespace net {

class Connection {
public:
    Connection();
    ~Connection();
    void open();
    void close();
    bool send(const std::string& payload);

    Connection& operator=(const Connection& other);

private:
    int fd;
};

struct Endpoint {
    std::string host;
    int port;
};

union Tag {
    int i;
    char c;
};

enum class State {
    Idle,
    Active,
    Closed,
};

typedef unsigned long ulong;
using Bytes = std::vector<unsigned char>;

template <typename T>
class Buffer {
public:
    Buffer() = default;
    void push(T value) { data.push_back(value); }
    T pop();

private:
    std::vector<T> data;
};

template <typename T>
T Buffer<T>::pop() {
    T v = data.back();
    data.pop_back();
    return v;
}

template <typename T>
concept Hashable = requires(T t) { t.hash(); };

void free_function(int x) {
    auto buf = new Buffer<int>();
    buf->push(x);
}

}  // namespace net
}  // namespace app

void app::net::Connection::open() {
    fd = 3;
}

void app::net::Connection::close() {
    fd = -1;
}

app::net::Connection::Connection() : fd(-1) {}

app::net::Connection::~Connection() {}
