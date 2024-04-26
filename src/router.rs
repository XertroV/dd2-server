use tokio::net::TcpStream;
use bitflags::bitflags;


struct Router {
    methods: Vec<Method>,
    db: Database,
}

impl Router {
    fn new(db: Database) -> Self {
        Router {
            methods: Vec::new(),
            db,
        }
    }

    fn add_method(mut self, method: Method) -> Self {
        self.methods.push(method);
        self
    }

    async fn run(&self, stream: TcpStream) {

    }
}

struct Database {

}

struct Method {
    name: String,
    handler: fn(Request) -> Response,
}

#[repr(u8)]
enum Request {
    Authenticate { token: String } = 1,
    ReportContext { context: PlayerCtx } = 2,
}


bitflags! {
    // todo: choose good coprimes
    // context is a big mix of many redundant variables. we can always divide by 2 if possible. Can obfuscate by multiplying numbers and shifting right.
    // also, send entire game camera nod every once in a while.
    pub struct CtxFlags: u64 {
        const IN_MAP       = 3;
        const IN_EDITOR    = 5;
        const IN_MT_EDITOR = 7;
        const IN_DD2_MAP   = 11;
        const IN_DD2_LIKE  = 13;
        const NOT_DD2      = 17;
    }
}

enum PlayerCtx {

}

struct Response {

}

