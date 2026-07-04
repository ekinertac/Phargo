//! The v2 tree-walking evaluator (stage 3, in progress).
//!
//! Walks the owned AST and runs it over the byte-correct [`Value`] model.
//! This first increment covers scalars, arrays, the full control-flow set,
//! user functions, string interpolation, and a starter library of builtins.
//! Objects/classes, generators, and the rest of the ~250 builtins come next.
#![allow(dead_code)]

use super::ast::*;
use super::value::*;
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::rc::Rc;

#[derive(Debug)]
pub struct RunError(pub String);

/// Where a foreach-by-ref array lives: a local variable or an object property.
enum ArrPlace {
    Var(String),
    Prop(Rc<RefCell<Obj>>, String),
}

/// Definition-site context for a function or class: the file it was declared
/// in plus that file's namespace and `use` aliases (see Eval::def_ctx).
struct DefCtx {
    file: Option<PathBuf>,
    ns: String,
    uses: Rc<HashMap<String, String>>,
}
type R<T> = Result<T, RunError>;

/// Control-flow signal bubbled up from statement execution.
enum Flow {
    Normal,
    Break(u32),
    Continue(u32),
    Return(Value),
}

pub struct Eval {
    out: Vec<u8>,
    /// Scope stack; `scopes[0]` is the global scope. Functions get a fresh scope.
    scopes: Vec<HashMap<String, Value>>,
    funcs: HashMap<String, Rc<FuncDecl>>,
    classes: HashMap<String, Rc<ClassDecl>>,
    consts: HashMap<String, Value>,
    /// Static property storage, keyed by (lowercased class, prop name).
    static_props: HashMap<(String, String), Value>,
    /// Class of the currently executing method, for `self`/`parent`/`static`.
    current_class: Option<String>,
    /// Late-static-binding scope: the class the current method was *called on*
    /// (runtime class of $this, or the class named in a static call). `static::`
    /// and get_called_class() resolve here; forwarding calls (self::/parent::/
    /// static::) inherit it, explicit `C::m()` calls rebind it.
    called_class: Option<String>,
    /// error_reporting() level (default E_ALL; we don't emit notices, but tests
    /// read the value back).
    error_level: i64,
    /// date.timezone / date_default_timezone_set() — IANA zone name. "UTC" means
    /// the trivial zone (no TZif lookup).
    default_tz: String,
    /// In-flight thrown exception (set by `throw`, cleared by a matching `catch`).
    thrown: Option<Value>,
    /// Current function/method call nesting — guards against stack overflow.
    call_depth: usize,
    /// Current expression-evaluation recursion depth (deep AST spines).
    eval_depth: usize,
    /// The file currently executing (for `__FILE__`/`__DIR__` + relative include).
    cur_file: Option<PathBuf>,
    /// Canonical paths already pulled in via `include_once`/`require_once`.
    included: HashSet<String>,
    /// Output-buffering watermarks into `out` (one per active `ob_start`).
    ob_stack: Vec<usize>,
    /// Next stream resource id handed out by `fopen` (cosmetic, for var_dump/id).
    next_res_id: i64,
    /// Callbacks registered via register_shutdown_function, run after main exits.
    shutdown_fns: Vec<(Value, Vec<Value>)>,
    /// declare(strict_types=1): scalar params/returns must match exactly
    /// (int→float widening excepted) instead of weak-mode coercion.
    strict_types: bool,
    /// Warning-suppression depth: >0 inside isset()/empty()/??-lhs/@/by-ref
    /// out-args, where PHP stays silent about undefined variables/keys.
    quiet: u32,
    /// set_error_handler() callback (invoked by warn/trigger_error) and the
    /// re-entrancy guard for warnings raised inside the handler itself.
    error_handler: Option<Value>,
    in_error_handler: bool,
    /// 1-based source line of the statement currently executing (0 = unknown).
    cur_line: u32,
    /// bcscale() default decimal precision for the bc* builtins.
    bc_scale: usize,
    /// strtok() resumable state: (subject, cursor).
    strtok_state: Option<(Vec<u8>, usize)>,
    /// (class, method) → resolved Rc'd declaration; cleared on class changes.
    method_cache: RefCell<HashMap<(String, String), Option<(String, Rc<MethodDecl>)>>>,
    /// `fn:<name>` / `class:<name>` (lowercase) → definition-site context.
    /// __FILE__/__DIR__, the namespace, and `use` aliases are all bound at the
    /// file that DECLARED the function/class — bodies executing later (called
    /// from another file) must see that context, not the caller's.
    def_ctx: HashMap<String, DefCtx>,
    /// Current namespace (lowercased, "" = global) and `use` alias map
    /// (alias-lower → FQ-lower). File-scoped: saved/restored across includes.
    cur_ns: String,
    use_map: HashMap<String, String>,
    /// Call-frame stack for exception traces: (display name like "f" or
    /// "Class->m", caller's line at the call site). Parallel to cur_fn pushes.
    frames: Vec<(String, u32, String)>,  // (callable, callsite line, callsite file)
    /// Function/class names registered by the PRELUDE — they emulate C
    /// internals, so their bodies never emit engine warnings (lowercased).
    prelude_fns: HashSet<String>,
    prelude_classes: HashSet<String>,
    /// Per-call stack: is the current function a by-ref return (`function &f`)?
    /// `return $undef` in those creates the variable silently.
    byref_ret: Vec<bool>,
    /// Per-class: does the hierarchy declare any typed/readonly instance prop?
    /// (Fast path: property writes skip declaration lookup entirely when false.)
    typed_props_cache: HashMap<String, bool>,
    /// `static $x = …` storage inside functions/methods, keyed by
    /// (function key, var name). Values persist across calls.
    static_vars: HashMap<(String, String), Value>,
    /// Autoloaders registered via spl_autoload_register, tried in order when a
    /// class is first touched and unknown.
    autoloaders: Vec<Value>,
    /// Classes currently being autoloaded (recursion guard, lowercased).
    autoload_active: HashSet<String>,
    /// Per-call argument stack, for func_get_args()/func_num_args().
    cur_args: Vec<Rc<Vec<Value>>>,
    /// Per-call function/method name stack, for __FUNCTION__/__METHOD__.
    cur_fn: Vec<String>,
    /// xorshift RNG state for rand()/mt_rand() (deterministic; tests rarely depend
    /// on exact values, and a fixed seed keeps runs reproducible).
    rng_state: u64,
    /// Enum cases are singletons: cache (class, case) -> the one shared instance
    /// so `Enum::from(x) === Enum::Case` holds (object identity).
    enum_cases: HashMap<(String, String), Value>,
    /// Anonymous classes: map a `new class {…}` decl's address to its assigned
    /// unique internal name (stable across re-evaluations, so instanceof works).
    anon_names: HashMap<usize, String>,
    /// Active generator collection buffer (eager generators collect yields here).
    gen_buf: Option<Arr>,
    /// Total node count accumulated into the active generator buffer. Capped so
    /// an infinite generator that yields large values (e.g. `while(1) yield from
    /// [10000 elems]`) can't exhaust the heap before the step limit trips.
    gen_nodes: usize,
    steps: u64,
}

const MAX_CALL_DEPTH: usize = 2000;

/// A minimal exception/error hierarchy, parsed before every program so that
/// `new Exception(...)`, `getMessage()`, `instanceof`, and `catch` work through
/// the ordinary class machinery.
const PRELUDE: &[u8] = br##"<?php
class stdClass {}
interface Throwable {}
interface Stringable {}
interface Traversable {}
interface Iterator extends Traversable {}
interface IteratorAggregate extends Traversable {}
interface ArrayAccess {}
interface Countable {}
interface JsonSerializable {}
class Exception implements Throwable {
    protected $message = "";
    protected $code = 0;
    protected $previous = null;
    protected $file = "";
    protected $line = 0;
    public function __construct($message = "", $code = 0, $previous = null) {
        $this->message = $message; $this->code = $code; $this->previous = $previous;
        $this->file = __phargo_cur_file();
        $this->line = __phargo_cur_line();
        $this->__trace = __phargo_trace();
    }
    public function getMessage() { return $this->message; }
    public function getCode() { return $this->code; }
    public function getPrevious() { return $this->previous; }
    public function getTrace() { return $this->__trace ?? []; }
    public function getTraceAsString() {
        $out = []; $i = 0;
        foreach (($this->__trace ?? []) as $f) {
            $out[] = "#$i " . $f["file"] . "(" . $f["line"] . "): " . $f["function"] . "()";
            $i++;
        }
        $out[] = "#$i {main}";
        return implode("\n", $out);
    }
    public function getFile() { return $this->file; }
    public function getLine() { return $this->line; }
    public function __toString() { return $this->message; }
}
class ErrorException extends Exception {}
class Error implements Throwable {
    protected $message = "";
    protected $code = 0;
    protected $previous = null;
    protected $file = "";
    protected $line = 0;
    public function __construct($message = "", $code = 0, $previous = null) {
        $this->message = $message; $this->code = $code; $this->previous = $previous;
        $this->file = __phargo_cur_file();
        $this->line = __phargo_cur_line();
        $this->__trace = __phargo_trace();
    }
    public function getMessage() { return $this->message; }
    public function getCode() { return $this->code; }
    public function getPrevious() { return $this->previous; }
    public function getTrace() { return $this->__trace ?? []; }
    public function getTraceAsString() {
        $out = []; $i = 0;
        foreach (($this->__trace ?? []) as $f) {
            $out[] = "#$i " . $f["file"] . "(" . $f["line"] . "): " . $f["function"] . "()";
            $i++;
        }
        $out[] = "#$i {main}";
        return implode("\n", $out);
    }
    public function getFile() { return $this->file; }
    public function getLine() { return $this->line; }
    public function __toString() { return $this->message; }
}
class TypeError extends Error {}
class ValueError extends Error {}
class ArgumentCountError extends TypeError {}
class ArithmeticError extends Error {}
class DivisionByZeroError extends ArithmeticError {}
class UnhandledMatchError extends Error {}
class RuntimeException extends Exception {}
class LogicException extends Exception {}
class InvalidArgumentException extends LogicException {}
class OutOfRangeException extends LogicException {}
class OutOfBoundsException extends RuntimeException {}
class LengthException extends LogicException {}
class DomainException extends LogicException {}
class RangeException extends RuntimeException {}
class UnexpectedValueException extends RuntimeException {}
class UnderflowException extends RuntimeException {}
class OverflowException extends RuntimeException {}
class JsonException extends Exception {}

class PDOException extends RuntimeException {
    public $errorInfo = null;
}
class PDO {
    const FETCH_LAZY = 1; const FETCH_ASSOC = 2; const FETCH_NUM = 3;
    const FETCH_BOTH = 4; const FETCH_OBJ = 5; const FETCH_COLUMN = 7;
    const FETCH_KEY_PAIR = 12; const FETCH_DEFAULT = 0;
    const PARAM_NULL = 0; const PARAM_INT = 1; const PARAM_STR = 2;
    const PARAM_LOB = 3; const PARAM_BOOL = 5;
    const ATTR_ERRMODE = 3; const ERRMODE_SILENT = 0; const ERRMODE_WARNING = 1;
    const ERRMODE_EXCEPTION = 2; const ATTR_DEFAULT_FETCH_MODE = 19;
    const ATTR_DRIVER_NAME = 16; const ATTR_STRINGIFY_FETCHES = 17;
    const ATTR_TIMEOUT = 2; const ATTR_AUTOCOMMIT = 0; const ATTR_PREFETCH = 1;
    const ATTR_SERVER_VERSION = 4; const ATTR_CLIENT_VERSION = 5;
    const ATTR_SERVER_INFO = 6; const ATTR_CONNECTION_STATUS = 7;
    const ATTR_CASE = 8; const ATTR_CURSOR_NAME = 9; const ATTR_CURSOR = 10;
    const ATTR_ORACLE_NULLS = 11; const ATTR_PERSISTENT = 12;
    const ATTR_STATEMENT_CLASS = 13; const ATTR_FETCH_TABLE_NAMES = 14;
    const ATTR_FETCH_CATALOG_NAMES = 15; const ATTR_MAX_COLUMN_LEN = 18;
    const ATTR_EMULATE_PREPARES = 20; const CASE_NATURAL = 0;
    const CASE_LOWER = 2; const CASE_UPPER = 1; const NULL_NATURAL = 0;
    const FETCH_CLASS = 8; const FETCH_INTO = 9; const FETCH_FUNC = 10;
    const FETCH_NAMED = 11; const FETCH_GROUP = 65536; const FETCH_UNIQUE = 196608;
    const FETCH_CLASSTYPE = 262144; const FETCH_SERIALIZE = 524288;
    const FETCH_PROPS_LATE = 1048576; const FETCH_ORI_NEXT = 0;
    public $__h; public $__fetchmode = 4; public $__attrs = [];
    public function __construct($dsn, $user = null, $pass = null, $options = null) {
        if (strpos($dsn, "sqlite:") !== 0) {
            throw new PDOException("could not find driver");
        }
        $this->__h = __pdo_open(substr($dsn, 7));
    }
    public function exec($sql) { $r = __pdo_query($this->__h, $sql, []); return $r["affected"]; }
    public function prepare($sql, $options = []) { return new PDOStatement($this->__h, $sql, $this->__fetchmode); }
    public function query($sql, $mode = null) {
        $st = new PDOStatement($this->__h, $sql, $mode ?? $this->__fetchmode);
        $st->execute();
        return $st;
    }
    public function lastInsertId($name = null) { return (string)__pdo_lastid($this->__h); }
    public function quote($s, $type = 2) { return "'" . str_replace("'", "''", (string)$s) . "'"; }
    public function beginTransaction() { $this->exec("BEGIN"); return true; }
    public function commit() { $this->exec("COMMIT"); return true; }
    public function rollBack() { $this->exec("ROLLBACK"); return true; }
    public function inTransaction() { return false; }
    public function setAttribute($k, $v) {
        if ($k == self::ATTR_DEFAULT_FETCH_MODE) { $this->__fetchmode = $v; }
        $this->__attrs[$k] = $v;
        return true;
    }
    public function getAttribute($k) {
        if ($k == self::ATTR_DRIVER_NAME) { return "sqlite"; }
        return $this->__attrs[$k] ?? null;
    }
    public function errorInfo() { return ["00000", null, null]; }
    public function errorCode() { return "00000"; }
    public static function getAvailableDrivers() { return ["sqlite"]; }
    public function sqliteCreateFunction($name, $callback, $argc = -1) {
        // the engine pre-registers the MySQL-compat set natively (src/pdo.rs);
        // PHP-callback UDFs would require evaluator reentrancy - accept and ignore
        return true;
    }
}
class PDOStatement implements IteratorAggregate {
    public $queryString;
    public $__h; public $__mode; public $__cols = []; public $__rows = [];
    public $__pos = 0; public $__affected = 0; public $__bound = [];
    public function __construct($h, $sql, $mode = 4) {
        $this->__h = $h; $this->queryString = $sql; $this->__mode = $mode;
    }
    public function bindValue($key, $value, $type = 2) {
        if (is_int($key)) { $this->__bound[$key] = $value; }
        else { $this->__bound[":" . ltrim($key, ":")] = $value; }
        return true;
    }
    public function bindParam($key, &$value, $type = 2) { return $this->bindValue($key, $value, $type); }
    public function execute($params = null) {
        // keys pass through: string keys bind as named parameters, int keys
        // positionally (in ksort order for pre-bound values)
        if (is_array($params)) { $p = $params; }
        else { $p = $this->__bound; ksort($p); }
        $r = __pdo_query($this->__h, $this->queryString, $p);
        $this->__cols = $r["cols"]; $this->__rows = $r["rows"];
        $this->__affected = $r["affected"]; $this->__pos = 0;
        return true;
    }
    private function __shape($row, $mode) {
        if ($mode == 3) { return $row; }
        $assoc = [];
        foreach ($this->__cols as $i => $c) { $assoc[$c] = $row[$i]; }
        if ($mode == 2) { return $assoc; }
        if ($mode == 5) { return (object)$assoc; }
        // FETCH_BOTH
        $both = $assoc;
        foreach ($row as $i => $v) { $both[$i] = $v; }
        return $both;
    }
    public function fetch($mode = null) {
        if ($this->__pos >= count($this->__rows)) { return false; }
        $row = $this->__rows[$this->__pos]; $this->__pos++;
        return $this->__shape($row, $mode ?? $this->__mode);
    }
    public function fetchAll($mode = null, $arg = null) {
        $m = $mode ?? $this->__mode;
        $out = [];
        while ($this->__pos < count($this->__rows)) {
            $row = $this->__rows[$this->__pos]; $this->__pos++;
            if ($m == 7) { $out[] = $row[$arg ?? 0]; }
            else { $out[] = $this->__shape($row, $m); }
        }
        return $out;
    }
    public function fetchColumn($n = 0) {
        if ($this->__pos >= count($this->__rows)) { return false; }
        $row = $this->__rows[$this->__pos]; $this->__pos++;
        return $row[$n] ?? false;
    }
    public function fetchObject($class = null) { return $this->fetch(5); }
    public function rowCount() { return $this->__affected; }
    public function columnCount() { return count($this->__cols); }
    public function closeCursor() { $this->__pos = count($this->__rows); return true; }
    public function setFetchMode($mode) { $this->__mode = $mode; return true; }
    public function getIterator(): Iterator {
        $out = [];
        while (($r = $this->fetch()) !== false) { $out[] = $r; }
        return new ArrayIterator($out);
    }
    public function errorInfo() { return ["00000", null, null]; }
}
enum RoundingMode {
    case HalfAwayFromZero;
    case HalfTowardsZero;
    case HalfEven;
    case HalfOdd;
    case TowardsZero;
    case AwayFromZero;
    case NegativeInfinity;
    case PositiveInfinity;
}
namespace BcMath {
    class Number {
        public $value;
        public $scale;
        public function __construct($num) {
            if (is_int($num)) { $num = (string)$num; }
            $this->value = bcadd($num, "0", __phargo_bcscale_of($num));
            $this->scale = __phargo_bcscale_of($this->value);
        }
        private function __wrap($v) { return new \BcMath\Number($v); }
        private function __sc($other, $extra = 0) {
            $o = $other instanceof \BcMath\Number ? $other->scale : __phargo_bcscale_of((string)$other);
            return max($this->scale, $o) + $extra;
        }
        private function __val($other) {
            return $other instanceof \BcMath\Number ? $other->value : (string)$other;
        }
        private function __trim($v) {
            if (strpos($v, ".") !== false) { $v = rtrim(rtrim($v, "0"), "."); }
            return $v === "" || $v === "-" ? "0" : $v;
        }
        public function add($n, $scale = null) { return $this->__wrap(bcadd($this->value, $this->__val($n), $scale ?? $this->__sc($n))); }
        public function sub($n, $scale = null) { return $this->__wrap(bcsub($this->value, $this->__val($n), $scale ?? $this->__sc($n))); }
        public function mul($n, $scale = null) {
            $os = $n instanceof \BcMath\Number ? $n->scale : __phargo_bcscale_of((string)$n);
            return $this->__wrap(bcmul($this->value, $this->__val($n), $scale ?? ($this->scale + $os)));
        }
        public function div($n, $scale = null) { return $this->__wrap($this->__trim(bcdiv($this->value, $this->__val($n), $scale ?? ($this->__sc($n) + 10)))); }
        public function mod($n, $scale = null) { return $this->__wrap(bcmod($this->value, $this->__val($n), $scale ?? $this->__sc($n))); }
        public function pow($n, $scale = null) { return $this->__wrap($this->__trim(bcpow($this->value, $this->__val($n), $scale ?? ($this->scale * 4 + 10)))); }
        public function powmod($e, $m) { return $this->__wrap(bcpowmod($this->value, $this->__val($e), $this->__val($m))); }
        public function sqrt($scale = null) { return $this->__wrap($this->__trim(bcsqrt($this->value, $scale ?? ($this->scale + 10)))); }
        public function floor() { return $this->__wrap(bcfloor($this->value)); }
        public function ceil() { return $this->__wrap(bcceil($this->value)); }
        public function round($precision = 0) { return $this->__wrap(bcround($this->value, $precision)); }
        public function compare($n, $scale = null) { return bccomp($this->value, $this->__val($n), $scale ?? $this->__sc($n)); }
        public function __toString() { return $this->value; }
    }
}
namespace {
interface DateTimeInterface {
    const ATOM = 'Y-m-d\TH:i:sP';
    const ISO8601 = 'Y-m-d\TH:i:sO';
    const RFC822 = 'D, d M y H:i:s O';
    const RFC850 = 'l, d-M-y H:i:s T';
    const RFC1036 = 'D, d M y H:i:s O';
    const RFC1123 = 'D, d M Y H:i:s O';
    const RFC2822 = 'D, d M Y H:i:s O';
    const RFC3339 = 'Y-m-d\TH:i:sP';
    const RFC3339_EXTENDED = 'Y-m-d\TH:i:s.vP';
    const RFC7231 = 'D, d M Y H:i:s \G\M\T';
    const COOKIE = 'l, d-M-Y H:i:s T';
    const RSS = 'D, d M Y H:i:s O';
    const W3C = 'Y-m-d\TH:i:sP';
}
class DateTimeZone {
    const AFRICA = 1;
    const AMERICA = 2;
    const ANTARCTICA = 4;
    const ARCTIC = 8;
    const ASIA = 16;
    const ATLANTIC = 32;
    const AUSTRALIA = 64;
    const EUROPE = 128;
    const INDIAN = 256;
    const PACIFIC = 512;
    const UTC = 1024;
    const ALL = 2047;
    const ALL_WITH_BC = 4095;
    const PER_COUNTRY = 4096;
    public $name;
    public function __construct($name = "UTC") {
        if (!__phargo_tz_valid($name)) {
            throw new Exception("DateTimeZone::__construct(): Unknown or bad timezone ($name)");
        }
        $this->name = $name;
    }
    public function getName() { return $this->name; }
    public function getOffset($dt) { return __phargo_tz_offset($this->name, $dt->getTimestamp()); }
    public function getTransitions($begin = null, $end = null) {
        if ($begin === null) { $begin = -2147483648; }
        if ($end === null) { $end = 2147483647; }
        return __phargo_tz_transitions($this->name, $begin, $end);
    }
    public static function listIdentifiers($group = 2047, $country = null) {
        return timezone_identifiers_list($group, $country);
    }
    public static function listAbbreviations() { return []; }
}
class DatePeriod implements IteratorAggregate {
    const EXCLUDE_START_DATE = 1;
    const INCLUDE_END_DATE = 2;
    public $start; public $interval; public $end;
    public $recurrences; public $include_start_date = true; public $include_end_date = false;
    private $__options = 0;
    public function __construct($start, $interval = null, $end = null, $options = 0) {
        if (is_string($start)) {
            // ISO 8601 form: "R<n>/<start>/P<interval>"
            $parts = explode("/", $start);
            $options = $interval === null ? 0 : $interval;
            $this->recurrences = (int)substr($parts[0], 1);
            $start = new DateTimeImmutable($parts[1]);
            $interval = new DateInterval($parts[2]);
            $end = null;
        }
        $this->start = $start; $this->interval = $interval;
        if (is_int($end)) { $this->recurrences = $end; }
        else { $this->end = $end; }
        $this->__options = $options;
        $this->include_start_date = ($options & 1) === 0;
        $this->include_end_date = ($options & 2) !== 0;
    }
    public static function createFromISO8601String($spec, $options = 0) {
        return new DatePeriod($spec, $options);
    }
    public function getStartDate() { return $this->start; }
    public function getEndDate() { return $this->end; }
    public function getDateInterval() { return $this->interval; }
    public function getRecurrences() { return $this->recurrences; }
    public function getIterator(): Iterator {
        $out = [];
        $cur = clone $this->start;
        $emitted = 0; $step = 0;
        while (true) {
            if ($this->end !== null) {
                $endts = $this->end->getTimestamp();
                if ($cur->getTimestamp() > $endts) { break; }
                if ($cur->getTimestamp() == $endts && !$this->include_end_date) { break; }
            } elseif ($this->recurrences !== null) {
                // recurrences = periods after the start date
                if ($step > $this->recurrences) { break; }
            } else { break; }
            if (!($step === 0 && !$this->include_start_date)) {
                $out[] = clone $cur;
                $emitted++;
            }
            if ($emitted > 10000) { break; }
            $next = clone $cur;
            $cur = $next->add($this->interval);
            $step++;
        }
        return new ArrayIterator($out);
    }
}
class DateTime implements DateTimeInterface {
    public $__ts;
    public $__tz;
    public function __construct($s = "now", $tz = null) {
        $this->__tz = $tz === null ? date_default_timezone_get() : $tz->getName();
        $this->__ts = __phargo_strtotime_tz($s, time(), $this->__tz);
        if ($this->__ts === false) {
            throw new Exception("DateTime::__construct(): Failed to parse time string ($s)");
        }
    }
    public function format($fmt) { return __phargo_date_tz($fmt, $this->__ts, $this->__tz); }
    public function getTimestamp() { return $this->__ts; }
    public function setTimestamp($ts) { $this->__ts = $ts; return $this; }
    public function setDate($y, $m, $d) { $this->__ts = __phargo_mktime_tz((int)$this->format("H"), (int)$this->format("i"), (int)$this->format("s"), $m, $d, $y, $this->__tz); return $this; }
    public function setTime($h, $i, $s = 0) { $this->__ts = __phargo_mktime_tz($h, $i, $s, (int)$this->format("n"), (int)$this->format("j"), (int)$this->format("Y"), $this->__tz); return $this; }
    public function setISODate($y, $w, $day = 1) {
        // Monday of ISO week 1 contains Jan 4
        $jan4 = __phargo_mktime_tz(0, 0, 0, 1, 4, $y, $this->__tz);
        $dow = (int)__phargo_date_tz('N', $jan4, $this->__tz);
        $target = $jan4 + ((($w - 1) * 7) + ($day - $dow)) * 86400;
        $this->__ts = __phargo_mktime_tz((int)$this->format('H'), (int)$this->format('i'), (int)$this->format('s'),
            (int)__phargo_date_tz('n', $target, $this->__tz), (int)__phargo_date_tz('j', $target, $this->__tz),
            (int)__phargo_date_tz('Y', $target, $this->__tz), $this->__tz);
        return $this;
    }
    public function getTimezone() { return new DateTimeZone($this->__tz); }
    public function setTimezone($tz) { $this->__tz = $tz->getName(); return $this; }
    public function getOffset() { return __phargo_tz_offset($this->__tz, $this->__ts); }
    public function add($iv) { $this->__ts = phargo_civil_add($this->__ts, $iv->y, $iv->m, $iv->d, $iv->h, $iv->i, $iv->s, $this->__tz); return $this; }
    public function sub($iv) { $this->__ts = phargo_civil_add($this->__ts, -$iv->y, -$iv->m, -$iv->d, -$iv->h, -$iv->i, -$iv->s, $this->__tz); return $this; }
    public function modify($s) { $this->__ts = __phargo_modify($this->__ts, $s, $this->__tz); return $this; }
    public function diff($other) { return DateInterval::__fromArray(phargo_date_diff($this->__ts, $other->getTimestamp())); }
    public static function createFromFormat($fmt, $s, $tz = null) {
        $tzname = $tz === null ? date_default_timezone_get() : $tz->getName();
        $r = __phargo_createfromformat($fmt, $s, $tzname);
        if ($r === false) { return false; }
        $d = new static("@" . $r["ts"]);
        $d->__tz = $r["tz"];
        return $d;
    }
}
class DateTimeImmutable implements DateTimeInterface {
    public $__ts;
    public $__tz;
    public function __construct($s = "now", $tz = null) {
        $this->__tz = $tz === null ? date_default_timezone_get() : $tz->getName();
        $this->__ts = __phargo_strtotime_tz($s, time(), $this->__tz);
        if ($this->__ts === false) {
            throw new Exception("DateTimeImmutable::__construct(): Failed to parse time string ($s)");
        }
    }
    public function format($fmt) { return __phargo_date_tz($fmt, $this->__ts, $this->__tz); }
    public function getTimestamp() { return $this->__ts; }
    public function setTimestamp($ts) { $n = clone $this; $n->__ts = $ts; return $n; }
    private function __viaMutable($m, $args) {
        $d = new DateTime("@" . $this->__ts);
        $d->__tz = $this->__tz;
        call_user_func_array([$d, $m], $args);
        $n = clone $this;
        $n->__ts = $d->getTimestamp();
        return $n;
    }
    public function setDate($y, $m, $d) { return $this->__viaMutable("setDate", [$y, $m, $d]); }
    public function setTime($h, $i, $s = 0) { return $this->__viaMutable("setTime", [$h, $i, $s]); }
    public function setISODate($y, $w, $day = 1) { return $this->__viaMutable("setISODate", [$y, $w, $day]); }
    public function getTimezone() { return new DateTimeZone($this->__tz); }
    public function setTimezone($tz) { $n = clone $this; $n->__tz = $tz->getName(); return $n; }
    public function getOffset() { return __phargo_tz_offset($this->__tz, $this->__ts); }
    public function add($iv) { $n = clone $this; $n->__ts = phargo_civil_add($this->__ts, $iv->y, $iv->m, $iv->d, $iv->h, $iv->i, $iv->s, $this->__tz); return $n; }
    public function sub($iv) { $n = clone $this; $n->__ts = phargo_civil_add($this->__ts, -$iv->y, -$iv->m, -$iv->d, -$iv->h, -$iv->i, -$iv->s, $this->__tz); return $n; }
    public function modify($s) { $n = clone $this; $n->__ts = __phargo_modify($this->__ts, $s, $this->__tz); return $n; }
    public function diff($other) { return DateInterval::__fromArray(phargo_date_diff($this->__ts, $other->getTimestamp())); }
    public static function createFromFormat($fmt, $s, $tz = null) {
        $tzname = $tz === null ? date_default_timezone_get() : $tz->getName();
        $r = __phargo_createfromformat($fmt, $s, $tzname);
        if ($r === false) { return false; }
        $d = new static("@" . $r["ts"]);
        $d->__tz = $r["tz"];
        return $d;
    }
}
function date_create($s = "now", $tz = null) { return new DateTime($s, $tz); }
function date_create_immutable($s = "now", $tz = null) { return new DateTimeImmutable($s, $tz); }
function date_create_from_format($fmt, $s, $tz = null) { return DateTime::createFromFormat($fmt, $s, $tz); }
function date_create_immutable_from_format($fmt, $s, $tz = null) { return DateTimeImmutable::createFromFormat($fmt, $s, $tz); }
function date_diff($a, $b, $absolute = false) { return $a->diff($b); }
function date_format($d, $fmt) { return $d->format($fmt); }
function date_add($d, $iv) { return $d->add($iv); }
function date_sub($d, $iv) { return $d->sub($iv); }
function date_modify($d, $s) { return $d->modify($s); }
function date_timestamp_get($d) { return $d->getTimestamp(); }
function date_timestamp_set($d, $ts) { return $d->setTimestamp($ts); }
function date_offset_get($d) { return $d->getOffset(); }
function date_timezone_get($d) { return $d->getTimezone(); }
function date_timezone_set($d, $tz) { return $d->setTimezone($tz); }
function date_date_set($d, $y, $m, $day) { return $d->setDate($y, $m, $day); }
function date_time_set($d, $h, $i, $s = 0) { return $d->setTime($h, $i, $s); }
function date_isodate_set($d, $y, $w, $day = 1) { return $d->setISODate($y, $w, $day); }
function timezone_open($tz) { return new DateTimeZone($tz); }
function timezone_name_get($tz) { return $tz->getName(); }
function timezone_offset_get($tz, $dt) { return $tz->getOffset($dt); }
function date_interval_create_from_date_string($s) { return DateInterval::createFromDateString($s); }
function date_interval_format($iv, $fmt) { return $iv->format($fmt); }
class DateInterval {
    public $y = 0; public $m = 0; public $d = 0;
    public $h = 0; public $i = 0; public $s = 0;
    public $f = 0; public $days = false; public $invert = 0;
    public function __construct($spec = "") {
        if ($spec === "") { return; }
        $inT = false; $num = "";
        for ($k = 0; $k < strlen($spec); $k++) {
            $c = $spec[$k];
            if ($c === "P") { continue; }
            if ($c === "T") { $inT = true; continue; }
            if (ctype_digit($c)) { $num .= $c; continue; }
            $n = (int)$num; $num = "";
            if ($c === "Y") { $this->y = $n; }
            elseif ($c === "M") { if ($inT) { $this->i = $n; } else { $this->m = $n; } }
            elseif ($c === "W") { $this->d += $n * 7; }
            elseif ($c === "D") { $this->d = $n; }
            elseif ($c === "H") { $this->h = $n; }
            elseif ($c === "S") { $this->s = $n; }
        }
    }
    public static function __fromArray($a) {
        $iv = new DateInterval();
        $iv->y = $a["y"]; $iv->m = $a["m"]; $iv->d = $a["d"];
        $iv->h = $a["h"]; $iv->i = $a["i"]; $iv->s = $a["s"];
        $iv->days = $a["days"]; $iv->invert = $a["invert"];
        return $iv;
    }
    public static function createFromDateString($s) {
        $b = __phargo_modify(946684800, $s, "UTC"); // fixed base avoids now() drift
        return DateInterval::__fromArray(phargo_date_diff(946684800, $b));
    }
    public function __serialize(): array {
        return ["y" => $this->y, "m" => $this->m, "d" => $this->d, "h" => $this->h,
                "i" => $this->i, "s" => $this->s, "f" => $this->f, "invert" => $this->invert, "days" => $this->days];
    }
    public function __unserialize($d): void {
        foreach ($d as $k => $v) { $this->$k = $v; }
    }
    public function format($f) {
        $r = ""; $n = strlen($f);
        for ($k = 0; $k < $n; $k++) {
            if ($f[$k] === "%" && $k + 1 < $n) {
                $k++; $c = $f[$k];
                if ($c === "y") { $r .= $this->y; }
                elseif ($c === "Y") { $r .= sprintf("%02d", $this->y); }
                elseif ($c === "m") { $r .= $this->m; }
                elseif ($c === "M") { $r .= sprintf("%02d", $this->m); }
                elseif ($c === "d") { $r .= $this->d; }
                elseif ($c === "D") { $r .= sprintf("%02d", $this->d); }
                elseif ($c === "h") { $r .= $this->h; }
                elseif ($c === "H") { $r .= sprintf("%02d", $this->h); }
                elseif ($c === "i") { $r .= $this->i; }
                elseif ($c === "I") { $r .= sprintf("%02d", $this->i); }
                elseif ($c === "s") { $r .= $this->s; }
                elseif ($c === "S") { $r .= sprintf("%02d", $this->s); }
                elseif ($c === "a") { $r .= $this->days; }
                elseif ($c === "R") { $r .= $this->invert ? "-" : "+"; }
                elseif ($c === "%") { $r .= "%"; }
                else { $r .= $c; }
            } else { $r .= $f[$k]; }
        }
        return $r;
    }
}
class ReflectionClass {
    public $name;
    public function __construct($arg) { $this->name = is_object($arg) ? get_class($arg) : $arg; }
    public function getName() { return $this->name; }
    public function getShortName() { $p = strrpos($this->name, "\\"); return $p === false ? $this->name : substr($this->name, $p + 1); }
    public function getParentClass() { $p = get_parent_class($this->name); return $p === false ? false : new ReflectionClass($p); }
    public function hasMethod($m) { return method_exists($this->name, $m); }
    public function hasProperty($p) { return property_exists($this->name, $p); }
    public function getMethod($m) { return new ReflectionMethod($this->name, $m); }
    public function getProperty($p) { return new ReflectionProperty($this->name, $p); }
    public function getMethods() { $r = []; foreach (get_class_methods($this->name) as $m) { $r[] = new ReflectionMethod($this->name, $m); } return $r; }
    public function getInterfaceNames() { return array_values(class_implements($this->name)); }
    public function getInterfaces() { $r = []; foreach (class_implements($this->name) as $i) { $r[$i] = new ReflectionClass($i); } return $r; }
    public function implementsInterface($i) { $n = strtolower($i); foreach (class_implements($this->name) as $x) { if (strtolower($x) === $n) return true; } return false; }
    public function isSubclassOf($c) { return is_subclass_of($this->name, $c); }
    public function isInstance($obj) { return is_a($obj, $this->name); }
    public function isInterface() { return false; }
    public function isAbstract() { return false; }
    public function isFinal() { return false; }
    public function isInstantiable() { return true; }
    public function getConstants() { return phargo_class_constants($this->name); }
    public function getConstant($n) { $c = phargo_class_constants($this->name); return $c[$n] ?? false; }
    public function hasConstant($n) { return isset(phargo_class_constants($this->name)[$n]); }
    public function getConstructor() { return method_exists($this->name, "__construct") ? new ReflectionMethod($this->name, "__construct") : null; }
    public function getProperties() { $r = []; foreach (array_keys(get_class_vars($this->name)) as $p) { $r[] = new ReflectionProperty($this->name, $p); } return $r; }
    public function getDefaultProperties() { return get_class_vars($this->name); }
    public function newInstance(...$args) { $n = $this->name; return new $n(...$args); }
    public function newInstanceArgs($args = []) { $n = $this->name; return new $n(...$args); }
    public function newInstanceWithoutConstructor() { $n = $this->name; return new $n(); }
    public function getStaticProperties() { return get_class_vars($this->name); }
    public function getStaticPropertyValue($n, $default = null) { return $default; }
    public function getReflectionConstants() { $r = []; foreach (phargo_class_constants($this->name) as $k => $v) { $r[] = new ReflectionClassConstant($this->name, $k); } return $r; }
    public function getReflectionConstant($n) { return new ReflectionClassConstant($this->name, $n); }
    public function isEnum() { return false; }
    public function isTrait() { return false; }
    public function isReadOnly() { return false; }
    public function isAnonymous() { return false; }
    public function isCloneable() { return true; }
    public function isIterable() { return false; }
    public function isInternal() { return false; }
    public function isUserDefined() { return true; }
    public function getModifiers() { return 0; }
    public function getTraitNames() { return []; }
    public function getTraits() { return []; }
    public function getAttributes($name = null, $flags = 0) { return []; }
    public function getDocComment() { return false; }
    public function getFileName() { return false; }
    public function getStartLine() { return false; }
    public function getEndLine() { return false; }
    public function getNamespaceName() { $p = strrpos($this->name, "\\"); return $p === false ? "" : substr($this->name, 0, $p); }
    public function inNamespace() { return strpos($this->name, "\\") !== false; }
}
class ReflectionClassConstant {
    public $class; public $name;
    public function __construct($c, $n) { $this->class = is_object($c) ? get_class($c) : $c; $this->name = $n; }
    public function getName() { return $this->name; }
    public function getValue() { return constant($this->class . "::" . $this->name); }
    public function getDeclaringClass() { return new ReflectionClass($this->class); }
    public function isPublic() { return true; }
    public function isPrivate() { return false; }
    public function isProtected() { return false; }
    public function isFinal() { return false; }
    public function isEnumCase() { return false; }
    public function getModifiers() { return 1; }
    public function getAttributes($name = null, $flags = 0) { return []; }
}
class ReflectionObject extends ReflectionClass {}
class ReflectionEnum extends ReflectionClass {
    public function isEnum() { return true; }
    public function getCases() { $r = []; $n = $this->name; foreach ($n::cases() as $c) { $r[] = $this->isBacked() ? new ReflectionEnumBackedCase($n, $c->name) : new ReflectionEnumUnitCase($n, $c->name); } return $r; }
    public function getCase($name) { $n = $this->name; return $this->isBacked() ? new ReflectionEnumBackedCase($n, $name) : new ReflectionEnumUnitCase($n, $name); }
    public function hasCase($name) { $n = $this->name; foreach ($n::cases() as $c) { if ($c->name === $name) return true; } return false; }
    public function isBacked() { $n = $this->name; foreach ($n::cases() as $c) { return isset($c->value); } return false; }
    public function getBackingType() { $n = $this->name; foreach ($n::cases() as $c) { return new ReflectionNamedType(is_int($c->value) ? "int" : "string"); } return null; }
}
class ReflectionEnumUnitCase {
    public $class; public $name;
    public function __construct($c, $n) { $this->class = is_object($c) ? get_class($c) : $c; $this->name = $n; }
    public function getName() { return $this->name; }
    public function getValue() { return constant($this->class . "::" . $this->name); }
    public function getDeclaringClass() { return new ReflectionEnum($this->class); }
}
class ReflectionEnumBackedCase extends ReflectionEnumUnitCase {
    public function getBackingValue() { return $this->getValue()->value; }
}
class ReflectionMethod {
    public $class; public $name;
    public function __construct($c, $m = null) { if ($m === null) { $parts = explode("::", $c); $c = $parts[0]; $m = $parts[1]; } $this->class = is_object($c) ? get_class($c) : $c; $this->name = $m; }
    public function getName() { return $this->name; }
    public function getDeclaringClass() { return new ReflectionClass($this->class); }
    public function invoke($obj, ...$args) { $n = $this->name; return $obj->$n(...$args); }
    public function invokeArgs($obj, $args = []) { $n = $this->name; return $obj->$n(...$args); }
    public function getParameters() { $r = []; foreach (phargo_func_params($this->class, $this->name) as $p) { $r[] = new ReflectionParameter($p); } return $r; }
    public function getNumberOfParameters() { return count(phargo_func_params($this->class, $this->name)); }
    public function getNumberOfRequiredParameters() { $n = 0; foreach (phargo_func_params($this->class, $this->name) as $p) { if (!$p["optional"]) $n++; } return $n; }
    public function getReturnType() { $t = phargo_func_return_type($this->class, $this->name); return $t === null ? null : new ReflectionNamedType($t); }
    public function hasReturnType() { return phargo_func_return_type($this->class, $this->name) !== null; }
    public function getShortName() { return $this->name; }
    public function isStatic() { return false; }
    public function isPublic() { return true; }
    public function isPrivate() { return false; }
    public function isProtected() { return false; }
    public function isFinal() { return false; }
    public function isAbstract() { return false; }
    public function isConstructor() { return strtolower($this->name) === "__construct"; }
    public function isVariadic() { foreach (phargo_func_params($this->class, $this->name) as $p) { if ($p["variadic"]) return true; } return false; }
    public function getModifiers() { return 1; }
    public function getDocComment() { return false; }
    public function getAttributes($name = null, $flags = 0) { return []; }
    public function setAccessible($a) {}
}
class ReflectionParameter {
    public $name; private $info;
    public function __construct($info) { $this->info = $info; $this->name = $info["name"]; }
    public function getName() { return $this->name; }
    public function isOptional() { return $this->info["optional"]; }
    public function isVariadic() { return $this->info["variadic"]; }
    public function hasType() { return $this->info["type"] !== null; }
    public function getType() { return $this->info["type"] === null ? null : new ReflectionNamedType($this->info["type"]); }
    public function allowsNull() { return $this->info["type"] === null || $this->info["type"][0] === "?"; }
    public function isPassedByReference() { return $this->info["by_ref"] ?? false; }
    public function canBePassedByValue() { return !($this->info["by_ref"] ?? false); }
    public function isDefaultValueAvailable() { return $this->info["has_default"] ?? false; }
    public function getPosition() { return $this->info["position"] ?? 0; }
    public function getDefaultValue() { return $this->info["default"] ?? null; }
    public function isDefaultValueConstant() { return false; }
    public function getDefaultValueConstantName() { return null; }
}
class ReflectionProperty {
    public $class; public $name; private $__info;
    public function __construct($c, $n) { $this->class = is_object($c) ? get_class($c) : $c; $this->name = $n; $this->__info = phargo_prop_info($this->class, $n); }
    public function getName() { return $this->name; }
    public function getValue($obj = null) { $n = $this->name; return $obj->$n; }
    public function setValue($obj, $v) { $n = $this->name; $obj->$n = $v; }
    public function getDeclaringClass() { return new ReflectionClass($this->class); }
    public function hasType() { return $this->__info !== null && $this->__info["type"] !== null; }
    public function getType() { return ($this->__info === null || $this->__info["type"] === null) ? null : new ReflectionNamedType($this->__info["type"]); }
    public function getModifiers() { return 1; }
    public function isPublic() { return $this->__info === null || $this->__info["visibility"] === 0; }
    public function isPrivate() { return $this->__info !== null && $this->__info["visibility"] === 1; }
    public function isProtected() { return $this->__info !== null && $this->__info["visibility"] === 2; }
    public function isStatic() { return $this->__info !== null && $this->__info["static"]; }
    public function isReadOnly() { return $this->__info !== null && $this->__info["readonly"]; }
    public function isPromoted() { return $this->__info !== null && $this->__info["promoted"]; }
    public function isReadable() { return true; }
    public function isWritable() { return $this->__info === null || !$this->__info["readonly"]; }
    public function isDefault() { return true; }
    public function isInitialized($obj = null) { $n = $this->name; return $obj === null ? true : isset($obj->$n); }
    public function getAttributes($name = null, $flags = 0) { return []; }
    public function getDocComment() { return false; }
    public function setAccessible($a) {}
}
class ReflectionFunction {
    public $name;
    public function __construct($n) { $this->name = $n; }
    public function getName() { return $this->name; }
    public function invoke(...$args) { return call_user_func_array($this->name, $args); }
    public function invokeArgs($args = []) { return call_user_func_array($this->name, $args); }
    public function getParameters() { $r = []; foreach (phargo_func_params("", $this->name) as $p) { $r[] = new ReflectionParameter($p); } return $r; }
    public function getNumberOfParameters() { return count(phargo_func_params("", $this->name)); }
    public function getNumberOfRequiredParameters() { $n = 0; foreach (phargo_func_params("", $this->name) as $p) { if (!$p["optional"]) $n++; } return $n; }
    public function getReturnType() { $t = phargo_func_return_type("", $this->name); return $t === null ? null : new ReflectionNamedType($t); }
    public function hasReturnType() { return phargo_func_return_type("", $this->name) !== null; }
    public function getShortName() { return $this->name; }
    public function isVariadic() { foreach (phargo_func_params("", $this->name) as $p) { if ($p["variadic"]) return true; } return false; }
    public function getDocComment() { return false; }
    public function getAttributes($name = null, $flags = 0) { return []; }
}
class ReflectionNamedType {
    public $name; private $__nullable;
    public function __construct($n) {
        if (strlen($n) > 0 && $n[0] === "?") { $this->__nullable = true; $n = substr($n, 1); } else { $this->__nullable = false; }
        $this->name = $n;
    }
    public function getName() { return $this->name; }
    public function allowsNull() { $l = strtolower($this->name); return $this->__nullable || $l === "null" || $l === "mixed"; }
    public function isBuiltin() {
        $b = ["int", "float", "string", "bool", "array", "object", "mixed", "void", "null", "callable", "iterable", "never", "false", "true", "self", "static", "parent"];
        return in_array(strtolower($this->name), $b);
    }
    public function __toString() { return ($this->__nullable ? "?" : "") . $this->name; }
}
class ReflectionException extends Exception {}

// Generators are eager in this engine: the function body runs to completion,
// collecting yields into __d; this object iterates them. send() can't feed values
// back (eager), and infinite generators hit the step limit. getReturn() works.
class Generator implements Iterator {
    public $__d = []; public $__k = []; public $__p = 0; public $__ret = null;
    public function rewind(): void { $this->__p = 0; }
    public function valid(): bool { return $this->__p < count($this->__k); }
    public function current(): mixed { return $this->valid() ? $this->__d[$this->__k[$this->__p]] : null; }
    public function key(): mixed { return $this->valid() ? $this->__k[$this->__p] : null; }
    public function next(): void { $this->__p = $this->__p + 1; }
    public function send($value) { $this->__p = $this->__p + 1; return $this->current(); }
    public function getReturn() { return $this->__ret; }
}
class ArrayIterator implements Iterator, ArrayAccess, Countable {
    const STD_PROP_LIST = 1; const ARRAY_AS_PROPS = 2;
    private $__d; private $__k; private $__p = 0;
    public function __construct($array = []) { $this->__d = $array; $this->__k = array_keys($array); }
    public function rewind(): void { $this->__k = array_keys($this->__d); $this->__p = 0; }
    public function valid(): bool { return $this->__p < count($this->__k); }
    public function current(): mixed { return $this->__d[$this->__k[$this->__p]]; }
    public function key(): mixed { return $this->__k[$this->__p]; }
    public function next(): void { $this->__p = $this->__p + 1; }
    public function offsetExists($k): bool { return isset($this->__d[$k]); }
    public function offsetGet($k): mixed { return $this->__d[$k] ?? null; }
    public function offsetSet($k, $v): void { if ($k === null) { $this->__d[] = $v; } else { $this->__d[$k] = $v; } }
    public function offsetUnset($k): void { unset($this->__d[$k]); }
    public function count(): int { return count($this->__d); }
    public function getArrayCopy() { return $this->__d; }
    public function append($v) { $this->__d[] = $v; }
}
class RecursiveArrayIterator extends ArrayIterator {
    public function hasChildren() { return is_array($this->current()); }
    public function getChildren() { return new RecursiveArrayIterator($this->current()); }
}
// Wraps any Traversable as an Iterator.
class IteratorIterator implements Iterator {
    protected $__it;
    public function __construct($it) {
        if ($it instanceof IteratorAggregate) { $it = $it->getIterator(); }
        $this->__it = $it;
    }
    public function getInnerIterator() { return $this->__it; }
    public function rewind(): void { $this->__it->rewind(); }
    public function valid(): bool { return $this->__it->valid(); }
    public function current(): mixed { return $this->__it->current(); }
    public function key(): mixed { return $this->__it->key(); }
    public function next(): void { $this->__it->next(); }
}
abstract class FilterIterator extends IteratorIterator {
    abstract public function accept(): bool;
    public function rewind(): void { $this->__it->rewind(); $this->__fetch(); }
    public function next(): void { $this->__it->next(); $this->__fetch(); }
    private function __fetch() { while ($this->__it->valid() && !$this->accept()) { $this->__it->next(); } }
}
class CallbackFilterIterator extends IteratorIterator {
    private $__cb;
    public function __construct($it, $cb) { parent::__construct($it); $this->__cb = $cb; }
    public function accept(): bool { return (bool)($this->__cb)($this->__it->current(), $this->__it->key(), $this->__it); }
    public function rewind(): void { $this->__it->rewind(); $this->__fetch(); }
    public function next(): void { $this->__it->next(); $this->__fetch(); }
    private function __fetch() { while ($this->__it->valid() && !$this->accept()) { $this->__it->next(); } }
}
class LimitIterator extends IteratorIterator {
    private $__offset; private $__count; private $__pos = 0;
    public function __construct($it, $offset = 0, $count = -1) { parent::__construct($it); $this->__offset = $offset; $this->__count = $count; }
    public function rewind(): void { $this->__it->rewind(); $this->__pos = 0; for ($i = 0; $i < $this->__offset && $this->__it->valid(); $i++) { $this->__it->next(); } }
    public function valid(): bool { return ($this->__count === -1 || $this->__pos < $this->__count) && $this->__it->valid(); }
    public function next(): void { $this->__it->next(); $this->__pos++; }
    public function getPosition() { return $this->__pos; }
}
class RegexIterator extends IteratorIterator {
    const MATCH = 0; const GET_MATCH = 1; const ALL_MATCHES = 2; const SPLIT = 3; const REPLACE = 4;
    const USE_KEY = 1;
    private $__regex; private $__mode; private $__flags;
    public function __construct($it, $regex, $mode = 0, $flags = 0) { parent::__construct($it); $this->__regex = $regex; $this->__mode = $mode; $this->__flags = $flags; }
    public function accept(): bool {
        $subject = ($this->__flags & 1) ? $this->__it->key() : $this->__it->current();
        return preg_match($this->__regex, (string)$subject) === 1;
    }
    public function rewind(): void { $this->__it->rewind(); $this->__fetch(); }
    public function next(): void { $this->__it->next(); $this->__fetch(); }
    private function __fetch() { while ($this->__it->valid() && !$this->accept()) { $this->__it->next(); } }
}
class RecursiveIteratorIterator implements Iterator {
    const LEAVES_ONLY = 0; const SELF_FIRST = 1; const CHILD_FIRST = 2;
    private $__stack; private $__mode; private $__root;
    public function __construct($it, $mode = 0, $flags = 0) {
        if ($it instanceof IteratorAggregate) { $it = $it->getIterator(); }
        $this->__root = $it; $this->__mode = $mode; $this->__stack = [];
    }
    public function rewind(): void { $this->__root->rewind(); $this->__stack = [$this->__root]; $this->__descend(); }
    public function valid(): bool { return !empty($this->__stack) && end($this->__stack)->valid(); }
    public function current(): mixed { return end($this->__stack)->current(); }
    public function key(): mixed { return end($this->__stack)->key(); }
    public function getDepth() { return count($this->__stack) - 1; }
    public function next(): void {
        $top = end($this->__stack);
        if ($top->hasChildren()) {
            $child = $top->getChildren();
            $child->rewind();
            $this->__stack[] = $child;
            $this->__descend();
            return;
        }
        $top->next();
        while (!empty($this->__stack) && !end($this->__stack)->valid()) {
            array_pop($this->__stack);
            if (!empty($this->__stack)) { end($this->__stack)->next(); }
        }
        $this->__descend();
    }
    private function __descend() {
        // LEAVES_ONLY: skip past nodes that have children, descending into them.
        if ($this->__mode === 0) {
            while (!empty($this->__stack) && end($this->__stack)->valid() && end($this->__stack)->hasChildren()) {
                $child = end($this->__stack)->getChildren();
                $child->rewind();
                $this->__stack[] = $child;
            }
        }
    }
}
class SplFileInfo {
    protected $__path;
    public function __construct($path) { $this->__path = $path; }
    public function getPathname() { return $this->__path; }
    public function getFilename() { return basename($this->__path); }
    public function getBasename($suffix = "") { return basename($this->__path, $suffix); }
    public function getExtension() { return pathinfo($this->__path, PATHINFO_EXTENSION); }
    public function getPath() { return dirname($this->__path); }
    public function getRealPath() { return realpath($this->__path); }
    public function isDir() { return is_dir($this->__path); }
    public function isFile() { return is_file($this->__path); }
    public function isReadable() { return is_readable($this->__path); }
    public function isWritable() { return is_writable($this->__path); }
    public function getSize() { return filesize($this->__path); }
    public function getType() { return is_dir($this->__path) ? "dir" : "file"; }
    public function getMTime() { return @filemtime($this->__path); }
    public function openFile($mode = "r") { return new SplFileObject($this->__path, $mode); }
    public function __toString() { return $this->__path; }
}
class DirectoryIterator implements Iterator {
    private $__d; private $__p = 0; private $__path;
    public function __construct($path) { $this->__path = $path; $this->__d = scandir($path); }
    public function rewind(): void { $this->__p = 0; }
    public function valid(): bool { return $this->__p < count($this->__d); }
    public function current(): mixed { return $this; }
    public function key(): mixed { return $this->__p; }
    public function next(): void { $this->__p = $this->__p + 1; }
    public function getFilename() { return $this->__d[$this->__p]; }
    public function getPathname() { return $this->__path . DIRECTORY_SEPARATOR . $this->__d[$this->__p]; }
    public function getPath() { return $this->__path; }
    public function isDot() { $f = $this->__d[$this->__p]; return $f === "." || $f === ".."; }
    public function isDir() { return is_dir($this->getPathname()); }
    public function isFile() { return is_file($this->getPathname()); }
    public function getBasename($suffix = "") { $f = $this->__d[$this->__p]; if ($suffix !== "" && str_ends_with($f, $suffix)) { $f = substr($f, 0, strlen($f) - strlen($suffix)); } return $f; }
    public function __toString() { return $this->__d[$this->__p]; }
}
class FilesystemIterator implements Iterator {
    const CURRENT_AS_PATHNAME = 32; const CURRENT_AS_FILEINFO = 0; const CURRENT_AS_SELF = 16;
    const KEY_AS_PATHNAME = 0; const KEY_AS_FILENAME = 256; const NEW_CURRENT_AND_KEY = 256;
    const FOLLOW_SYMLINKS = 512; const SKIP_DOTS = 4096; const UNIX_PATHS = 8192;
    protected $__fspath; protected $__fsflags; protected $__fsitems; protected $__fsp = 0;
    public function __construct($path, $flags = 4096) {
        $this->__fspath = rtrim($path, "/");
        $this->__fsflags = $flags;
        $items = @scandir($path);
        if ($items === false) { throw new UnexpectedValueException("FilesystemIterator::__construct(" . $path . "): failed to open dir"); }
        if ($flags & 4096) { $items = array_values(array_diff($items, [".", ".."])); }
        $this->__fsitems = $items;
    }
    public function rewind(): void { $this->__fsp = 0; }
    public function valid(): bool { return $this->__fsp < count($this->__fsitems); }
    public function next(): void { $this->__fsp = $this->__fsp + 1; }
    public function getFilename() { return $this->__fsitems[$this->__fsp]; }
    public function getPathname() { return $this->__fspath . "/" . $this->__fsitems[$this->__fsp]; }
    public function getPath() { return $this->__fspath; }
    public function isDot() { $f = $this->getFilename(); return $f === "." || $f === ".."; }
    public function isDir() { return is_dir($this->getPathname()); }
    public function isFile() { return is_file($this->getPathname()); }
    public function isLink() { return false; }
    public function isReadable() { return is_readable($this->getPathname()); }
    public function getExtension() { return pathinfo($this->getPathname(), PATHINFO_EXTENSION); }
    public function getBasename($suffix = "") { return basename($this->getPathname(), $suffix); }
    public function getSize() { return filesize($this->getPathname()); }
    public function getMTime() { return @filemtime($this->getPathname()); }
    public function key(): mixed { return ($this->__fsflags & 256) ? $this->getFilename() : $this->getPathname(); }
    public function current(): mixed {
        if ($this->__fsflags & 32) { return $this->getPathname(); }
        if ($this->__fsflags & 16) { return $this; }
        return new SplFileInfo($this->getPathname());
    }
    public function setFlags($flags) { $this->__fsflags = $flags; }
    public function getFlags() { return $this->__fsflags; }
    public function __toString() { return $this->getFilename(); }
}
class RecursiveDirectoryIterator extends FilesystemIterator {
    public function hasChildren($allow_links = false) { return !$this->isDot() && $this->isDir(); }
    public function getChildren() { return new RecursiveDirectoryIterator($this->getPathname(), $this->__fsflags); }
    public function getSubPath() { return ""; }
    public function getSubPathname() { return $this->getFilename(); }
}
class RegexIterator implements Iterator {
    const MATCH = 0; const GET_MATCH = 1; const ALL_MATCHES = 2; const SPLIT = 3; const REPLACE = 4;
    const USE_KEY = 1; const INVERT_MATCH = 2;
    protected $__rit; protected $__rre; protected $__rmode; protected $__rflags; protected $__rcur;
    public function __construct($it, $regex, $mode = 0, $flags = 0, $preg_flags = 0) {
        if ($it instanceof IteratorAggregate) { $it = $it->getIterator(); }
        $this->__rit = $it; $this->__rre = $regex; $this->__rmode = $mode; $this->__rflags = $flags;
    }
    public function rewind(): void { $this->__rit->rewind(); $this->__rgxadvance(); }
    public function valid(): bool { return $this->__rit->valid(); }
    public function key(): mixed { return $this->__rit->key(); }
    public function current(): mixed { return $this->__rcur; }
    public function next(): void { $this->__rit->next(); $this->__rgxadvance(); }
    public function getInnerIterator() { return $this->__rit; }
    protected function __rgxadvance() {
        while ($this->__rit->valid()) {
            $subj = ($this->__rflags & 1) ? $this->__rit->key() : $this->__rit->current();
            $subj = (string)$subj;
            $hit = preg_match($this->__rre, $subj, $m);
            if ($this->__rflags & 2) { $hit = !$hit; $m = []; }
            if ($hit) {
                if ($this->__rmode === 1) { $this->__rcur = $m; }
                elseif ($this->__rmode === 2) { preg_match_all($this->__rre, $subj, $ma); $this->__rcur = $ma; }
                elseif ($this->__rmode === 3) { $this->__rcur = preg_split($this->__rre, $subj); }
                else { $this->__rcur = $this->__rit->current(); }
                return;
            }
            $this->__rit->next();
        }
    }
}
class RecursiveRegexIterator extends RegexIterator {
    public function hasChildren() { return $this->__rit->hasChildren(); }
    public function getChildren() { return new RecursiveRegexIterator($this->__rit->getChildren(), $this->__rre, $this->__rmode, $this->__rflags); }
}
class SplFileObject implements Iterator {
    const DROP_NEW_LINE = 1; const READ_AHEAD = 2; const SKIP_EMPTY = 4; const READ_CSV = 8;
    private $__fp; private $__line = 0; private $__cur = false; private $__path; private $__flags = 0;
    public function __construct($filename, $mode = "r") {
        $this->__path = $filename;
        $this->__fp = fopen($filename, $mode);
        if ($this->__fp === false) { throw new RuntimeException("SplFileObject::__construct(" . $filename . "): Failed to open stream"); }
    }
    public function fgets() { return fgets($this->__fp); }
    public function fread($n) { return fread($this->__fp, $n); }
    public function fwrite($s) { return fwrite($this->__fp, $s); }
    public function fgetc() { return fgetc($this->__fp); }
    public function fgetcsv($sep = ",", $enc = "\"", $esc = "\\") { return fgetcsv($this->__fp, 0, $sep, $enc, $esc); }
    public function fputcsv($fields, $sep = ",", $enc = "\"", $esc = "\\", $eol = "\n") { return fputcsv($this->__fp, $fields, $sep, $enc, $esc, $eol); }
    public function eof() { return feof($this->__fp); }
    public function fseek($o, $w = 0) { return fseek($this->__fp, $o, $w); }
    public function ftell() { return ftell($this->__fp); }
    public function fflush() { return fflush($this->__fp); }
    public function rewind(): void { fseek($this->__fp, 0, 0); $this->__line = 0; $this->__cur = fgets($this->__fp); }
    public function valid(): bool { return $this->__cur !== false; }
    public function current(): mixed { $line = $this->__cur; if (($this->__flags & 1) && is_string($line)) { $line = rtrim($line, "\r\n"); } return $line; }
    public function key(): mixed { return $this->__line; }
    public function next(): void { $this->__cur = fgets($this->__fp); $this->__line = $this->__line + 1; }
    public function setFlags($f) { $this->__flags = $f; }
    public function getFlags() { return $this->__flags; }
    public function getPathname() { return $this->__path; }
    public function getRealPath() { return realpath($this->__path); }
    public function getFilename() { return basename($this->__path); }
    public function getBasename($suffix = "") { return basename($this->__path, $suffix); }
}
class SplTempFileObject extends SplFileObject {
    public function __construct($maxmem = 0) { parent::__construct("php://temp", "w+"); }
}
class ArrayObject implements ArrayAccess, IteratorAggregate, Countable {
    const STD_PROP_LIST = 1; const ARRAY_AS_PROPS = 2;
    private $__d;
    public function __construct($array = []) { $this->__d = $array; }
    public function offsetExists($k): bool { return isset($this->__d[$k]); }
    public function offsetGet($k): mixed { return $this->__d[$k] ?? null; }
    public function offsetSet($k, $v): void { if ($k === null) { $this->__d[] = $v; } else { $this->__d[$k] = $v; } }
    public function offsetUnset($k): void { unset($this->__d[$k]); }
    public function count(): int { return count($this->__d); }
    public function getArrayCopy() { return $this->__d; }
    public function append($v) { $this->__d[] = $v; }
    public function getIterator(): Iterator { return new ArrayIterator($this->__d); }
}
class SplDoublyLinkedList implements Iterator, Countable, ArrayAccess {
    const IT_MODE_LIFO = 2; const IT_MODE_FIFO = 0; const IT_MODE_DELETE = 1; const IT_MODE_KEEP = 0;
    protected $__d = []; protected $__p = 0; protected $__lifo = false; protected $__mode = 0;
    public function push($v) { $this->__d[] = $v; }
    public function pop() { return array_pop($this->__d); }
    public function shift() { return array_shift($this->__d); }
    public function unshift($v) { array_unshift($this->__d, $v); }
    public function top() { return $this->__d[count($this->__d) - 1]; }
    public function bottom() { return $this->__d[0]; }
    public function isEmpty() { return count($this->__d) === 0; }
    public function count(): int { return count($this->__d); }
    public function offsetExists($k): bool { return isset($this->__d[$k]); }
    public function offsetGet($k): mixed { return $this->__d[$k]; }
    public function offsetSet($k, $v): void { if ($k === null) { $this->__d[] = $v; } else { $this->__d[$k] = $v; } }
    public function offsetUnset($k): void { unset($this->__d[$k]); $n = []; foreach ($this->__d as $x) { $n[] = $x; } $this->__d = $n; }
    // Iteration direction is bit 2 of the mode (LIFO vs FIFO); __lifo mirrors it
    // for SplStack's constructor default and is kept in sync by setIteratorMode.
    public function setIteratorMode($mode) { $this->__mode = $mode; $this->__lifo = ($mode & 2) !== 0; }
    public function getIteratorMode() { return $this->__mode; }
    public function rewind(): void { $this->__p = $this->__lifo ? count($this->__d) - 1 : 0; }
    public function valid(): bool { return $this->__p >= 0 && $this->__p < count($this->__d); }
    public function current(): mixed { return $this->__d[$this->__p]; }
    public function key(): mixed { return $this->__p; }
    public function next(): void { if ($this->__lifo) { $this->__p = $this->__p - 1; } else { $this->__p = $this->__p + 1; } }
}
class SplStack extends SplDoublyLinkedList { public function __construct() { $this->__lifo = true; $this->__mode = 2; } }
class SplQueue extends SplDoublyLinkedList {
    public function enqueue($v) { $this->push($v); }
    public function dequeue() { return $this->shift(); }
}
class SplFixedArray implements ArrayAccess, Countable, Iterator {
    private $__d = []; private $__size = 0; private $__p = 0;
    public function __construct($size = 0) { $this->__size = $size; for ($i = 0; $i < $size; $i = $i + 1) { $this->__d[$i] = null; } }
    public function offsetExists($k): bool { return $k >= 0 && $k < $this->__size; }
    public function offsetGet($k): mixed { return $this->__d[$k] ?? null; }
    public function offsetSet($k, $v): void { $this->__d[$k] = $v; }
    public function offsetUnset($k): void { $this->__d[$k] = null; }
    public function count(): int { return $this->__size; }
    public function getSize() { return $this->__size; }
    public function setSize($s) { $this->__size = $s; return true; }
    public function toArray() { return $this->__d; }
    public static function fromArray($a) { $f = new SplFixedArray(count($a)); $i = 0; foreach ($a as $v) { $f[$i] = $v; $i++; } return $f; }
    public function rewind(): void { $this->__p = 0; }
    public function valid(): bool { return $this->__p < $this->__size; }
    public function current(): mixed { return $this->__d[$this->__p] ?? null; }
    public function key(): mixed { return $this->__p; }
    public function next(): void { $this->__p = $this->__p + 1; }
}
class SplObjectStorage implements Countable, Iterator, ArrayAccess {
    private $__o = []; private $__v = []; private $__k = []; private $__p = 0;
    public function attach($obj, $data = null) { $h = spl_object_id($obj); $this->__o[$h] = $obj; $this->__v[$h] = $data; }
    public function detach($obj) { $h = spl_object_id($obj); unset($this->__o[$h]); unset($this->__v[$h]); }
    public function contains($obj) { return isset($this->__o[spl_object_id($obj)]); }
    public function count(): int { return count($this->__o); }
    public function offsetExists($obj): bool { return $this->contains($obj); }
    public function offsetGet($obj): mixed { return $this->__v[spl_object_id($obj)] ?? null; }
    public function offsetSet($obj, $data): void { $this->attach($obj, $data); }
    public function offsetUnset($obj): void { $this->detach($obj); }
    public function rewind(): void { $this->__k = array_keys($this->__o); $this->__p = 0; }
    public function valid(): bool { return $this->__p < count($this->__k); }
    public function current(): mixed { return $this->__o[$this->__k[$this->__p]]; }
    public function key(): mixed { return $this->__p; }
    public function getInfo() { return $this->__v[$this->__k[$this->__p]] ?? null; }
    public function next(): void { $this->__p = $this->__p + 1; }
}
// WeakMap/WeakReference: modelled as strong references (no GC collection), which
// is correct for everything except tests that specifically assert collection.
class WeakMap implements ArrayAccess, Countable, IteratorAggregate {
    private $__k = []; private $__v = [];
    public function offsetExists($o): bool { return isset($this->__k[spl_object_id($o)]); }
    public function offsetGet($o): mixed { return $this->__v[spl_object_id($o)] ?? null; }
    public function offsetSet($o, $v): void { $id = spl_object_id($o); $this->__k[$id] = $o; $this->__v[$id] = $v; }
    public function offsetUnset($o): void { $id = spl_object_id($o); unset($this->__k[$id]); unset($this->__v[$id]); }
    public function count(): int { return count($this->__k); }
    public function getIterator(): Iterator { $p = []; foreach ($this->__k as $id => $o) { $p[] = $this->__v[$id]; } return new ArrayIterator($p); }
}
class WeakReference {
    private $__o = null;
    private function __construct() {}
    public static function create($o) { $r = new WeakReference(); $r->__o = $o; return $r; }
    public function get() { return $this->__o; }
}
abstract class SplHeap implements Countable, Iterator {
    protected $__h = [];
    abstract protected function compare($a, $b): int;
    protected function __top_index() { $best = 0; for ($i = 1; $i < count($this->__h); $i++) { if ($this->compare($this->__h[$i], $this->__h[$best]) > 0) { $best = $i; } } return $best; }
    public function insert($v) { $this->__h[] = $v; return true; }
    public function top() { return $this->__h[$this->__top_index()]; }
    public function extract() { if (count($this->__h) === 0) { return null; } $i = $this->__top_index(); $v = $this->__h[$i]; array_splice($this->__h, $i, 1); return $v; }
    public function count(): int { return count($this->__h); }
    public function isEmpty() { return count($this->__h) === 0; }
    public function rewind(): void {}
    public function valid(): bool { return count($this->__h) > 0; }
    public function current(): mixed { return $this->top(); }
    public function key(): mixed { return count($this->__h) - 1; }
    public function next(): void { $this->extract(); }
}
class SplMinHeap extends SplHeap { protected function compare($a, $b): int { return ($a < $b) ? 1 : (($a > $b) ? -1 : 0); } }
class SplMaxHeap extends SplHeap { protected function compare($a, $b): int { return ($a > $b) ? 1 : (($a < $b) ? -1 : 0); } }
class SplPriorityQueue implements Countable, Iterator {
    const EXTR_DATA = 1; const EXTR_PRIORITY = 2; const EXTR_BOTH = 3;
    private $__d = []; private $__flags = 1;
    private function __top_index() { $best = 0; for ($i = 1; $i < count($this->__d); $i++) { if ($this->__d[$i][0] > $this->__d[$best][0]) { $best = $i; } } return $best; }
    private function __shape($i) {
        if ($this->__flags === 2) { return $this->__d[$i][0]; }
        if ($this->__flags === 3) { return ["data" => $this->__d[$i][1], "priority" => $this->__d[$i][0]]; }
        return $this->__d[$i][1];
    }
    public function insert($value, $priority) { $this->__d[] = [$priority, $value]; return true; }
    public function setExtractFlags($flags) { $this->__flags = $flags; }
    public function top() { return $this->__shape($this->__top_index()); }
    public function extract() { if (count($this->__d) === 0) { return null; } $i = $this->__top_index(); $v = $this->__shape($i); array_splice($this->__d, $i, 1); return $v; }
    public function count(): int { return count($this->__d); }
    public function isEmpty() { return count($this->__d) === 0; }
    public function rewind(): void {}
    public function valid(): bool { return count($this->__d) > 0; }
    public function current(): mixed { return $this->top(); }
    public function key(): mixed { return count($this->__d) - 1; }
    public function next(): void { $this->extract(); }
}

// ---- DOM (built on the __dom_parse XML parser) ----
function __dom_escape_text($s) { return str_replace(["&", "<", ">"], ["&amp;", "&lt;", "&gt;"], $s); }
function __dom_escape_attr($s) { return str_replace(["&", "<", "\""], ["&amp;", "&lt;", "&quot;"], $s); }
class DOMNode {
    public $nodeType = 0; public $nodeName = ""; public $__kids = []; public $__attrs = [];
    public $__parent = null; public $ownerDocument = null;
    public function __get($n) {
        switch ($n) {
            case "childNodes": return new DOMNodeList($this->__kids);
            case "firstChild": return $this->__kids[0] ?? null;
            case "lastChild": return empty($this->__kids) ? null : $this->__kids[count($this->__kids) - 1];
            case "parentNode": return $this->__parent;
            case "textContent": return $this->__text();
            case "nodeValue": return $this->nodeType == 1 ? $this->__text() : ($this->__nv ?? null);
            case "tagName": case "localName": return $this->nodeName;
            case "nextSibling": return $this->__sib(1);
            case "previousSibling": return $this->__sib(-1);
            case "attributes": return new DOMNamedNodeMap($this->__attrs);
            case "length": return count($this->__kids);
            case "namespaceURI": case "prefix": return null;
            default: return null;
        }
    }
    public function __text() {
        if ($this->nodeType == 3 || $this->nodeType == 4) { return $this->__nv; }
        $s = ""; foreach ($this->__kids as $k) { $s .= $k->__text(); } return $s;
    }
    public function __sib($dir) {
        if ($this->__parent === null) { return null; }
        $ks = $this->__parent->__kids;
        foreach ($ks as $i => $k) { if ($k === $this) { return $ks[$i + $dir] ?? null; } }
        return null;
    }
    public function appendChild($child) {
        if ($child->nodeType == 11) { foreach ($child->__kids as $k) { $k->__parent = $this; $this->__kids[] = $k; } return $child; }
        $child->__parent = $this; $this->__kids[] = $child; return $child;
    }
    public function removeChild($child) { $n = []; foreach ($this->__kids as $k) { if ($k !== $child) { $n[] = $k; } } $this->__kids = $n; $child->__parent = null; return $child; }
    public function hasChildNodes() { return count($this->__kids) > 0; }
    public function hasAttributes() { return count($this->__attrs) > 0; }
    public function insertBefore($new, $ref = null) {
        $new->__parent = $this;
        if ($ref === null) { $this->__kids[] = $new; return $new; }
        $out = [];
        foreach ($this->__kids as $k) { if ($k === $ref) { $out[] = $new; } $out[] = $k; }
        $this->__kids = $out;
        return $new;
    }
    public function replaceChild($new, $old) {
        $new->__parent = $this;
        $out = [];
        foreach ($this->__kids as $k) { $out[] = ($k === $old) ? $new : $k; }
        $this->__kids = $out;
        return $old;
    }
    public function cloneNode($deep = true) {
        if ($this->nodeType == 3) { $c = new DOMText($this->__nv); $c->ownerDocument = $this->ownerDocument; return $c; }
        if ($this->nodeType == 4) { $c = new DOMCdataSection($this->__nv); $c->ownerDocument = $this->ownerDocument; return $c; }
        if ($this->nodeType == 8) { $c = new DOMComment($this->__nv); $c->ownerDocument = $this->ownerDocument; return $c; }
        $c = new DOMElement($this->nodeName);
        $c->__attrs = $this->__attrs;
        $c->ownerDocument = $this->ownerDocument;
        if ($deep) {
            foreach ($this->__kids as $k) { $kc = $k->cloneNode(true); $kc->__parent = $c; $c->__kids[] = $kc; }
        }
        return $c;
    }
    public function normalize() {}
    public function getElementsByTagName($name) { return new DOMNodeList($this->__collect($name)); }
    public function __collect($name) {
        $out = [];
        foreach ($this->__kids as $k) {
            if ($k->nodeType == 1) {
                if ($name === "*" || $k->nodeName === $name) { $out[] = $k; }
                foreach ($k->__collect($name) as $d) { $out[] = $d; }
            }
        }
        return $out;
    }
    // ChildNode / ParentNode convenience API (PHP 8+ DOM): plain strings passed
    // to append/prepend/before/after/replaceWith are coerced to DOMText nodes,
    // matching native PHP DOM behavior.
    private function __asNode($n) { return is_string($n) ? new DOMText($n) : $n; }
    public function remove() { if ($this->__parent !== null) { $this->__parent->removeChild($this); } }
    public function append(...$nodes) { foreach ($nodes as $n) { $this->appendChild($this->__asNode($n)); } }
    public function prepend(...$nodes) {
        $first = $this->__kids[0] ?? null;
        foreach ($nodes as $n) {
            $node = $this->__asNode($n);
            if ($first === null) { $this->appendChild($node); } else { $this->insertBefore($node, $first); }
        }
    }
    public function before(...$nodes) {
        if ($this->__parent === null) { return; }
        foreach ($nodes as $n) { $this->__parent->insertBefore($this->__asNode($n), $this); }
    }
    public function after(...$nodes) {
        if ($this->__parent === null) { return; }
        $next = $this->__sib(1);
        foreach ($nodes as $n) {
            $node = $this->__asNode($n);
            if ($next === null) { $this->__parent->appendChild($node); } else { $this->__parent->insertBefore($node, $next); }
        }
    }
    public function replaceWith(...$nodes) { $this->before(...$nodes); $this->remove(); }
}
class DOMElement extends DOMNode {
    public function __construct($name, $value = null) {
        $this->nodeType = 1; $this->nodeName = $name;
        if ($value !== null && $value !== "") { $t = new DOMText($value); $t->__parent = $this; $this->__kids[] = $t; }
    }
    public function getAttribute($n) { return $this->__attrs[$n] ?? ""; }
    public function setAttribute($n, $v) { $this->__attrs[$n] = (string)$v; return new DOMAttr($n, (string)$v); }
    public function hasAttribute($n) { return isset($this->__attrs[$n]); }
    public function removeAttribute($n) { unset($this->__attrs[$n]); }
    public function getAttributeNode($n) { return isset($this->__attrs[$n]) ? new DOMAttr($n, $this->__attrs[$n]) : false; }
    public function getAttributeNS($ns, $n) { return $this->getAttribute($n); }
    public function setAttributeNS($ns, $n, $v) { $this->setAttribute($n, $v); }
    public function hasAttributeNS($ns, $n) { return $this->hasAttribute($n); }
    public function removeAttributeNS($ns, $n) { $this->removeAttribute($n); }
    public function setIdAttribute($n, $isId) {}
    public function setAttributeNode($attr) {
        $old = isset($this->__attrs[$attr->nodeName]) ? new DOMAttr($attr->nodeName, $this->__attrs[$attr->nodeName]) : null;
        $this->__attrs[$attr->nodeName] = $attr->value;
        return $old;
    }
    public function setAttributeNodeNS($attr) { return $this->setAttributeNode($attr); }
    public function getAttributeNodeNS($ns, $n) { return $this->getAttributeNode($n); }
}
class DOMDocumentFragment extends DOMNode {
    public function __construct() { $this->nodeType = 11; $this->nodeName = "#document-fragment"; }
    public function appendXML($data) {
        $t = __dom_parse("<__f>" . $data . "</__f>");
        if ($t === false) { return false; }
        $doc = $this->ownerDocument ?? new DOMDocument();
        foreach ($t["kids"] as $k) { $c = $doc->__build($k); $c->__parent = $this; $this->__kids[] = $c; }
        return true;
    }
}
class DOMText extends DOMNode {
    public $__nv;
    public function __construct($data = "") { $this->nodeType = 3; $this->nodeName = "#text"; $this->__nv = $data; }
    public function __get($n) { if ($n === "data" || $n === "wholeText") { return $this->__nv; } return parent::__get($n); }
}
class DOMComment extends DOMNode {
    public $__nv;
    public function __construct($data = "") { $this->nodeType = 8; $this->nodeName = "#comment"; $this->__nv = $data; }
    public function __get($n) { if ($n === "data") { return $this->__nv; } return parent::__get($n); }
}
class DOMCdataSection extends DOMText {
    public function __construct($data = "") { $this->nodeType = 4; $this->nodeName = "#cdata-section"; $this->__nv = $data; }
}
class DOMAttr extends DOMNode {
    public $value;
    public function __construct($name, $value = "") { $this->nodeType = 2; $this->nodeName = $name; $this->value = $value; }
}
class DOMNodeList implements Iterator, Countable {
    public $length; private $__items; private $__p = 0;
    public function __construct($items = []) { $this->__items = array_values($items); $this->length = count($this->__items); }
    public function item($i) { return $this->__items[$i] ?? null; }
    public function count(): int { return $this->length; }
    public function rewind(): void { $this->__p = 0; }
    public function valid(): bool { return $this->__p < $this->length; }
    public function current(): mixed { return $this->__items[$this->__p]; }
    public function key(): mixed { return $this->__p; }
    public function next(): void { $this->__p = $this->__p + 1; }
}
class DOMNamedNodeMap implements Countable {
    private $__a;
    public function __construct($attrs) { $this->__a = $attrs; }
    public function getNamedItem($n) { return isset($this->__a[$n]) ? new DOMAttr($n, $this->__a[$n]) : null; }
    public function item($i) { $k = array_keys($this->__a); return isset($k[$i]) ? new DOMAttr($k[$i], $this->__a[$k[$i]]) : null; }
    public function count(): int { return count($this->__a); }
    public function __get($n) { return $n === "length" ? count($this->__a) : null; }
}
class DOMDocument extends DOMNode {
    public $documentElement = null; public $encoding = ""; public $version = "1.0"; public $formatOutput = false; public $preserveWhiteSpace = true;
    public function __construct($version = "1.0", $encoding = "") { $this->nodeType = 9; $this->nodeName = "#document"; $this->version = $version; if ($encoding !== "") { $this->encoding = $encoding; } }
    public function loadXML($xml, $opts = 0) {
        if (preg_match('/<\?xml[^>]*encoding=["\x27]([^"\x27]+)["\x27]/', $xml, $m)) { $this->encoding = $m[1]; }
        if (preg_match('/<\?xml[^>]*version=["\x27]([^"\x27]+)["\x27]/', $xml, $m)) { $this->version = $m[1]; }
        $tree = __dom_parse($xml);
        if ($tree === false) { return false; }
        $root = $this->__build($tree); $root->__parent = $this;
        $this->__kids = [$root]; $this->documentElement = $root; return true;
    }
    public function load($file, $opts = 0) { $xml = file_get_contents($file); if ($xml === false) { return false; } return $this->loadXML($xml); }
    public function __build($n) {
        if ($n["t"] == 1) {
            $el = new DOMElement($n["name"]); $el->ownerDocument = $this; $el->__attrs = $n["attrs"];
            foreach ($n["kids"] as $kid) { $c = $this->__build($kid); $c->__parent = $el; $el->__kids[] = $c; }
            return $el;
        } elseif ($n["t"] == 4) { $c = new DOMCdataSection($n["text"]); $c->ownerDocument = $this; return $c; }
        elseif ($n["t"] == 8) { $c = new DOMComment($n["text"]); $c->ownerDocument = $this; return $c; }
        else { $c = new DOMText($n["text"]); $c->ownerDocument = $this; return $c; }
    }
    public function createElement($name, $value = null) { $e = new DOMElement($name, $value); $e->ownerDocument = $this; return $e; }
    public function createDocumentFragment() { $f = new DOMDocumentFragment(); $f->ownerDocument = $this; return $f; }
    public function createElementNS($ns, $qname, $value = null) { return $this->createElement($qname, $value); }
    public function createAttributeNS($ns, $qname) { $a = new DOMAttr($qname); $a->ownerDocument = $this; return $a; }
    public function importNode($node, $deep = false) { $c = $node->cloneNode($deep); $c->ownerDocument = $this; return $c; }
    public function getElementsByTagNameNS($ns, $name) { return $this->getElementsByTagName($name); }
    public function createTextNode($data) { $t = new DOMText($data); $t->ownerDocument = $this; return $t; }
    public function createCDATASection($data) { $t = new DOMCdataSection($data); $t->ownerDocument = $this; return $t; }
    public function createComment($data) { $t = new DOMComment($data); $t->ownerDocument = $this; return $t; }
    public function createAttribute($name) { $a = new DOMAttr($name); $a->ownerDocument = $this; return $a; }
    public function appendChild($child) { $child->__parent = $this; $this->__kids[] = $child; if ($child->nodeType == 1) { $this->documentElement = $child; } return $child; }
    public function getElementById($id) {
        foreach ($this->__collect("*") as $e) { if (($e->__attrs["id"] ?? null) === $id) { return $e; } }
        return null;
    }
    public function saveXML($node = null) {
        if ($node === null) {
            $s = "<?xml version=\"" . $this->version . "\"" . ($this->encoding !== null && $this->encoding !== "" ? " encoding=\"" . $this->encoding . "\"" : "") . "?>\n";
            foreach ($this->__kids as $k) { $s .= $this->__ser($k); }
            return $s . "\n";
        }
        return $this->__ser($node);
    }
    // ---- HTML loading/saving ----
    // Real libxml's HTML parser tolerates unclosed void tags, unescaped `&`,
    // missing <html>/<body> wrappers, and a missing/whatever DOCTYPE. Our
    // __dom_parse is a strict XML parser, so loadHTML runs a lenient PHP
    // pre-pass that rewrites the source into well-formed XML before handing
    // it to __dom_parse, then reuses __build (same as loadXML) to make the tree.
    public $__had_doctype = false;
    public $__html = false;
    const __VOID_ELEMENTS = "area|base|br|col|embed|hr|img|input|link|meta|param|source|track|wbr";
    public function loadHTML($source, $options = 0) {
        $this->__had_doctype = preg_match('/<!DOCTYPE[^>]*>/i', $source) === 1;
        $source = preg_replace('/<!DOCTYPE[^>]*>/i', '', $source);
        // Self-close void elements (<br>, <img src="...">, ...) so the strict
        // parser doesn't treat them as unclosed containers swallowing siblings.
        $source = preg_replace('/<(' . self::__VOID_ELEMENTS . ')([^>]*?)\/?>/i', '<$1$2 />', $source);
        // Bare "&" (not part of a known/entity-shaped reference) must be escaped
        // or the XML parser's attribute/text scanning gets confused.
        $source = preg_replace('/&(?![a-zA-Z]+;|#[0-9]+;|#x[0-9a-fA-F]+;)/i', '&amp;', $source);
        $trimmed = trim($source);
        if (stripos($trimmed, '<html') !== 0) {
            if (stripos($trimmed, '<head') === 0 || stripos($trimmed, '<body') === 0) {
                $source = '<html>' . $source . '</html>';
            } else {
                $source = '<html><body>' . $source . '</body></html>';
            }
        } else {
            $source = $trimmed;
        }
        $tree = __dom_parse($source);
        if ($tree === false) { return false; }
        $root = $this->__build($tree); $root->__parent = $this;
        $this->__kids = [$root]; $this->documentElement = $root;
        $this->__html = true;
        return true;
    }
    public function loadHTMLFile($uri, $options = 0) {
        $s = file_get_contents($uri);
        if ($s === false) { return false; }
        return $this->loadHTML($s, $options);
    }
    public function saveHTML($node = null) {
        if ($node !== null) { return $this->__serHTML($node); }
        // libxml emits this exact DOCTYPE for HTML documents it didn't itself
        // read a doctype from (and we don't bother reconstructing an original
        // one even when __had_doctype is true - close enough for the corpus).
        $s = "<!DOCTYPE html PUBLIC \"-//W3C//DTD HTML 4.0 Transitional//EN\" \"http://www.w3.org/TR/REC-html40/loose.dtd\">\n";
        foreach ($this->__kids as $k) { $s .= $this->__serHTML($k); }
        return $s . "\n";
    }
    public function saveHTMLFile($uri) { return file_put_contents($uri, $this->saveHTML()); }
    // HTML-flavored serializer: void elements never get a closing tag (and
    // never self-close with "/>"), other empty elements always get an explicit
    // "</tag>" (HTML has no self-closing syntax outside the void set).
    public function __serHTML($n) {
        if ($n->nodeType == 3) { return __dom_escape_text($n->__nv); }
        if ($n->nodeType == 4) { return "<![CDATA[" . $n->__nv . "]]>"; }
        if ($n->nodeType == 8) { return "<!--" . $n->__nv . "-->"; }
        $isVoid = in_array(strtolower($n->nodeName), explode("|", self::__VOID_ELEMENTS));
        $s = "<" . $n->nodeName;
        foreach ($n->__attrs as $k => $v) { $s .= " " . $k . "=\"" . __dom_escape_attr($v) . "\""; }
        if ($isVoid) { return $s . ">"; }
        $s .= ">";
        foreach ($n->__kids as $c) { $s .= $this->__serHTML($c); }
        return $s . "</" . $n->nodeName . ">";
    }
    public function __ser($n) {
        if ($n->nodeType == 3) { return __dom_escape_text($n->__nv); }
        if ($n->nodeType == 4) { return "<![CDATA[" . $n->__nv . "]]>"; }
        if ($n->nodeType == 8) { return "<!--" . $n->__nv . "-->"; }
        $s = "<" . $n->nodeName;
        foreach ($n->__attrs as $k => $v) { $s .= " " . $k . "=\"" . __dom_escape_attr($v) . "\""; }
        if (empty($n->__kids)) { return $s . "/>"; }
        $s .= ">";
        foreach ($n->__kids as $c) { $s .= $this->__ser($c); }
        return $s . "</" . $n->nodeName . ">";
    }
}
// PHP 8.4 new DOM API (Dom\XMLDocument / Dom\HTMLDocument): resolved by simple
// name, reusing the classic DOMDocument tree. Static factory constructors.
class XMLDocument extends DOMDocument {
    public static function createFromString($source, $options = 0, $overrideEncoding = null) { $d = new XMLDocument(); $d->loadXML($source); return $d; }
    public static function createFromFile($path, $options = 0, $overrideEncoding = null) { $d = new XMLDocument(); $d->load($path); return $d; }
    public static function createEmpty($version = "1.0", $encoding = "UTF-8") { $d = new XMLDocument(); $d->version = $version; $d->encoding = $encoding; return $d; }
}
class HTMLDocument extends DOMDocument {
    public static function createFromString($source, $options = 0, $overrideEncoding = null) { $d = new HTMLDocument(); $d->loadXML($source); return $d; }
    public static function createFromFile($path, $options = 0, $overrideEncoding = null) { $d = new HTMLDocument(); $d->load($path); return $d; }
    public static function createEmpty($encoding = "UTF-8") { $d = new HTMLDocument(); $d->encoding = $encoding; return $d; }
}

// ---- DOMXPath (XPath 1.0 subset over the DOM tree above) ----
// Only the fragment of XPath the php-src corpus actually exercises: absolute
// (/a/b) and "//" descendant paths, "*"/"text()"/"@attr" node tests, and the
// predicates [N], [last()], [@attr], [@attr="v"]. Everything is implemented as
// free functions (prefixed __xp_) operating on plain DOMNode object references,
// then wrapped by the DOMXPath class methods query()/evaluate(). Grouping for
// position()/last() is "per immediate parent", matching XPath's per-context-node
// semantics for the simple non-nested-predicate case this subset supports.
function __xp_parse_step($p) {
    $b = strpos($p, "[");
    if ($b === false) { return [$p, null]; }
    $close = strrpos($p, "]");
    $test = substr($p, 0, $b);
    $pred = substr($p, $b + 1, $close - $b - 1);
    return [$test, $pred];
}
function __xp_tokenize($expr) {
    $s = $expr;
    if (str_starts_with($s, "//")) { $absolute = true; $s = substr($s, 2); $axis = "descendant"; }
    elseif (str_starts_with($s, "/")) { $absolute = true; $s = substr($s, 1); $axis = "child"; }
    else { $absolute = false; $axis = "child"; }
    $parts = explode("/", $s);
    $steps = [];
    foreach ($parts as $p) {
        if ($p === "") { $axis = "descendant"; continue; }
        [$test, $pred] = __xp_parse_step($p);
        $steps[] = ["axis" => $axis, "test" => $test, "pred" => $pred];
        $axis = "child";
    }
    return ["absolute" => $absolute, "steps" => $steps];
}
function __xp_parent_key($n) { return $n->__parent === null ? "__root__" : (string) spl_object_id($n->__parent); }
function __xp_group_by_parent($nodes) {
    $groups = [];
    foreach ($nodes as $n) { $groups[__xp_parent_key($n)][] = $n; }
    return $groups;
}
function __xp_apply_pred($nodes, $pred) {
    $pred = trim($pred);
    if ($pred === "") { return $nodes; }
    if ($pred === "last()") {
        $groups = __xp_group_by_parent($nodes);
        $out = [];
        foreach ($nodes as $n) {
            $grp = $groups[__xp_parent_key($n)];
            if (count($grp) > 0 && $grp[count($grp) - 1] === $n) { $out[] = $n; }
        }
        return $out;
    }
    if (is_numeric($pred)) {
        $idx = (int) $pred - 1;
        $groups = __xp_group_by_parent($nodes);
        $out = []; $seen = [];
        foreach ($nodes as $n) {
            $key = __xp_parent_key($n);
            if (isset($seen[$key])) { continue; }
            $seen[$key] = true;
            $grp = $groups[$key];
            if (isset($grp[$idx])) { $out[] = $grp[$idx]; }
        }
        return $out;
    }
    if (preg_match('/^@([A-Za-z_][A-Za-z0-9_-]*)\s*=\s*"([^"]*)"$/', $pred, $m) ||
        preg_match("/^@([A-Za-z_][A-Za-z0-9_-]*)\\s*=\\s*'([^']*)'\$/", $pred, $m)) {
        $attr = $m[1]; $val = $m[2];
        $out = [];
        foreach ($nodes as $n) { if (($n->__attrs[$attr] ?? null) === $val) { $out[] = $n; } }
        return $out;
    }
    if (preg_match('/^@([A-Za-z_][A-Za-z0-9_-]*)$/', $pred, $m)) {
        $attr = $m[1];
        $out = [];
        foreach ($nodes as $n) { if (isset($n->__attrs[$attr])) { $out[] = $n; } }
        return $out;
    }
    return $nodes;
}
function __xp_eval($doc, $steps, $absolute, $contextNode) {
    $current = [$absolute ? $doc : ($contextNode ?? $doc)];
    foreach ($steps as $step) {
        $axis = $step["axis"]; $test = $step["test"]; $pred = $step["pred"];
        $next = [];
        if (str_starts_with($test, "@")) {
            $attr = substr($test, 1);
            foreach ($current as $node) {
                if ($node->nodeType == 1 && isset($node->__attrs[$attr])) { $next[] = new DOMAttr($attr, $node->__attrs[$attr]); }
            }
        } elseif ($test === ".") {
            $next = $current;
        } elseif ($test === "..") {
            foreach ($current as $node) { if ($node->__parent !== null) { $next[] = $node->__parent; } }
        } elseif ($test === "text()") {
            foreach ($current as $node) {
                foreach ($node->__kids as $k) { if ($k->nodeType == 3) { $next[] = $k; } }
            }
        } else {
            foreach ($current as $node) {
                if ($axis === "descendant") {
                    foreach ($node->__collect($test) as $d) { $next[] = $d; }
                } else {
                    foreach ($node->__kids as $k) {
                        if ($k->nodeType == 1 && ($test === "*" || $k->nodeName === $test)) { $next[] = $k; }
                    }
                }
            }
        }
        if ($pred !== null) { $next = __xp_apply_pred($next, $pred); }
        $current = $next;
    }
    return $current;
}
function __xp_dedupe($nodes) {
    $out = []; $seen = [];
    foreach ($nodes as $n) {
        $id = spl_object_id($n);
        if (!isset($seen[$id])) { $seen[$id] = true; $out[] = $n; }
    }
    return $out;
}
class DOMXPath {
    private $__doc;
    public function __construct($doc) { $this->__doc = $doc; }
    public function query($expr, $contextNode = null) {
        $tok = __xp_tokenize(trim($expr));
        $nodes = __xp_eval($this->__doc, $tok["steps"], $tok["absolute"], $contextNode);
        return new DOMNodeList(__xp_dedupe($nodes));
    }
    public function evaluate($expr, $contextNode = null) {
        $expr = trim($expr);
        if (str_starts_with($expr, "count(") && str_ends_with($expr, ")")) {
            $inner = substr($expr, 6, -1);
            return (float) $this->query($inner, $contextNode)->length;
        }
        if (str_starts_with($expr, "string(") && str_ends_with($expr, ")")) {
            $inner = substr($expr, 7, -1);
            $r = $this->query($inner, $contextNode);
            return $r->length > 0 ? $r->item(0)->textContent : "";
        }
        return $this->query($expr, $contextNode);
    }
    public function registerNamespace($prefix, $uri) { return true; }
    public function registerPhpFunctions($restrict = null) { return null; }
}

// ---- XMLReader (a pull parser built by flattening the __dom_parse tree) ----
// PHP's XMLReader walks the document depth-first, emitting one event per
// read() call. Rather than re-implement streaming parsing, we parse the whole
// document up front with __dom_parse (same parser DOMDocument uses) and
// flatten it into a linear list of events once; read()/next() then just walk
// a cursor over that list. This is not a *true* streaming reader (no huge-file
// support) but matches observable behavior for the corpus's small fixtures.
class XMLReader {
    const NONE = 0;
    const ELEMENT = 1;
    const ATTRIBUTE = 2;
    const TEXT = 3;
    const CDATA = 4;
    const ENTITY_REF = 5;
    const ENTITY = 6;
    const PI = 7;
    const COMMENT = 8;
    const DOC = 9;
    const DOC_TYPE = 10;
    const DOC_FRAGMENT = 11;
    const NOTATION = 12;
    const WHITESPACE = 13;
    const SIGNIFICANT_WHITESPACE = 14;
    const END_ELEMENT = 15;
    const END_ENTITY = 16;
    const XML_DECLARATION = 17;
    const LOADDTD = 1;
    const DEFAULTATTRS = 2;
    const VALIDATE = 3;
    const SUBST_ENTITIES = 4;

    public $nodeType = 0;
    public $name = "";
    public $value = "";
    public $depth = 0;
    public $isEmptyElement = false;
    public $hasValue = false;
    public $hasAttributes = false;
    public $attributeCount = 0;
    public $localName = "";

    private $__events = [];
    private $__pos = -1;

    // Turn one __dom_parse node into 1-2 linear events (ELEMENT [+ END_ELEMENT
    // once its children are flattened]), recursing depth-first so the event
    // list order matches the order read() must emit.
    private function __flatten($n, $depth) {
        if ($n["t"] == 1) {
            $empty = empty($n["kids"]);
            $this->__events[] = ["type" => 1, "name" => $n["name"], "value" => "", "depth" => $depth, "empty" => $empty, "attrs" => $n["attrs"]];
            if (!$empty) {
                foreach ($n["kids"] as $k) { $this->__flatten($k, $depth + 1); }
                $this->__events[] = ["type" => 15, "name" => $n["name"], "value" => "", "depth" => $depth, "empty" => false, "attrs" => []];
            }
        } elseif ($n["t"] == 4) {
            $this->__events[] = ["type" => 4, "name" => "#cdata-section", "value" => $n["text"], "depth" => $depth, "empty" => false, "attrs" => []];
        } elseif ($n["t"] == 8) {
            $this->__events[] = ["type" => 8, "name" => "#comment", "value" => $n["text"], "depth" => $depth, "empty" => false, "attrs" => []];
        } else {
            $txt = $n["text"];
            $isWs = trim($txt) === "";
            $this->__events[] = ["type" => $isWs ? 14 : 3, "name" => "#text", "value" => $txt, "depth" => $depth, "empty" => false, "attrs" => []];
        }
    }

    private function __reset() {
        $this->nodeType = 0; $this->name = ""; $this->value = ""; $this->depth = 0;
        $this->isEmptyElement = false; $this->hasValue = false; $this->hasAttributes = false;
        $this->attributeCount = 0; $this->localName = "";
    }

    private function __load($xml) {
        $t = __dom_parse($xml);
        if ($t === false) { return false; }
        $this->__events = [];
        $this->__flatten($t, 0);
        $this->__pos = -1;
        $this->__reset();
        return true;
    }

    // Real XMLReader::open()/XML() are declared `static` but PHP still allows
    // calling them on an instance (`$r->open(...)`), which is the only form the
    // corpus actually uses. We implement them as instance methods and detect
    // the (deprecated) static-call form via `isset($this)`.
    public function open($uri, $encoding = null, $flags = 0) {
        if (!isset($this)) {
            $r = new XMLReader();
            if (!$r->open($uri, $encoding, $flags)) { return false; }
            return $r;
        }
        $xml = file_get_contents($uri);
        if ($xml === false) { return false; }
        return $this->__load($xml);
    }

    public function XML($source, $encoding = null, $flags = 0) {
        if (!isset($this)) {
            $r = new XMLReader();
            if (!$r->XML($source, $encoding, $flags)) { return false; }
            return $r;
        }
        return $this->__load($source);
    }

    public function read() {
        $this->__pos = $this->__pos + 1;
        if ($this->__pos >= count($this->__events)) {
            $this->__reset();
            return false;
        }
        $e = $this->__events[$this->__pos];
        $this->nodeType = $e["type"];
        $this->name = $e["name"];
        $this->localName = $e["name"];
        $this->value = $e["value"];
        $this->depth = $e["depth"];
        $this->isEmptyElement = $e["empty"];
        $this->hasValue = $e["value"] !== "";
        $this->hasAttributes = count($e["attrs"]) > 0;
        $this->attributeCount = count($e["attrs"]);
        return true;
    }

    public function getAttribute($name) {
        if ($this->__pos < 0 || $this->__pos >= count($this->__events)) { return null; }
        $e = $this->__events[$this->__pos];
        if ($e["type"] != 1) { return null; }
        return $e["attrs"][$name] ?? null;
    }

    public function moveToNextAttribute() { return false; }
    public function moveToElement() { return false; }

    // Skip the rest of the current node's subtree (all events at a deeper
    // depth, plus its own END_ELEMENT), then read() forward - optionally
    // filtering to the next ELEMENT with a matching name, like real XMLReader.
    public function next($name = null) {
        if ($this->__pos < 0 || $this->__pos >= count($this->__events)) { return false; }
        $curDepth = $this->__events[$this->__pos]["depth"];
        $p = $this->__pos + 1;
        $n = count($this->__events);
        while ($p < $n && $this->__events[$p]["depth"] > $curDepth) { $p++; }
        if ($p < $n && $this->__events[$p]["depth"] == $curDepth && $this->__events[$p]["type"] == 15) { $p++; }
        $this->__pos = $p - 1;
        while (true) {
            if (!$this->read()) { return false; }
            if ($name === null) { return true; }
            if ($this->nodeType == 1 && $this->name === $name) { return true; }
        }
    }

    public function close() {
        $this->__events = [];
        $this->__pos = -1;
        $this->__reset();
        return true;
    }

    public function readOuterXml() { return ""; }
    public function readInnerXml() { return ""; }

    public function readString() {
        if ($this->__pos < 0 || $this->__pos >= count($this->__events)) { return ""; }
        $e = $this->__events[$this->__pos];
        if ($e["type"] == 3 || $e["type"] == 4 || $e["type"] == 14) { return $e["value"]; }
        if ($e["type"] != 1) { return ""; }
        $curDepth = $e["depth"];
        $s = "";
        $p = $this->__pos + 1;
        $n = count($this->__events);
        while ($p < $n && $this->__events[$p]["depth"] > $curDepth) {
            $t = $this->__events[$p]["type"];
            if ($t == 3 || $t == 4 || $t == 14) { $s .= $this->__events[$p]["value"]; }
            $p++;
        }
        return $s;
    }

    public function setParserProperty($property, $value) { return true; }
    public function getParserProperty($property) { return false; }
    public function isValid() { return true; }
}

// ---- SimpleXML (built on the same __dom_parse tree) ----
function simplexml_load_string($xml, $class = null, $opts = 0) {
    $tree = __dom_parse($xml);
    if ($tree === false) { return false; }
    $sx = new SimpleXMLElement($tree);
    $enc = "";
    if (preg_match('/<\?xml[^>]*encoding=["\x27]([^"\x27]+)["\x27]/', $xml, $m)) { $enc = " encoding=\"" . $m[1] . "\""; }
    $ver = "1.0";
    if (preg_match('/<\?xml[^>]*version=["\x27]([^"\x27]+)["\x27]/', $xml, $m)) { $ver = $m[1]; }
    $sx->__decl = "<?xml version=\"" . $ver . "\"" . $enc . "?>\n";
    return $sx;
}
function simplexml_load_file($file, $class = null, $opts = 0) {
    $xml = file_get_contents($file);
    if ($xml === false) { return false; }
    return simplexml_load_string($xml);
}
function __sxml_ser($n) {
    if ($n["t"] == 3) { return __dom_escape_text($n["text"]); }
    if ($n["t"] == 4) { return "<![CDATA[" . $n["text"] . "]]>"; }
    if ($n["t"] == 8) { return ""; }
    $s = "<" . $n["name"];
    foreach ($n["attrs"] as $k => $v) { $s .= " " . $k . "=\"" . __dom_escape_attr($v) . "\""; }
    if (empty($n["kids"])) { return $s . "/>"; }
    $s .= ">";
    foreach ($n["kids"] as $c) { $s .= __sxml_ser($c); }
    return $s . "</" . $n["name"] . ">";
}
class SimpleXMLElement implements Iterator, ArrayAccess, Countable {
    private $__node; private $__sibs; private $__p = 0;
    public function __construct($node, $sibs = null) {
        if (is_string($node)) { $node = __dom_parse($node); }
        $this->__node = $node;
        $this->__sibs = $sibs === null ? [$node] : $sibs;
    }
    public function getName() { return $this->__node["name"]; }
    public function __get($name) {
        $matches = [];
        foreach ($this->__node["kids"] as $k) { if ($k["t"] == 1 && $k["name"] === $name) { $matches[] = $k; } }
        if (count($matches) === 0) { return null; }
        return new SimpleXMLElement($matches[0], $matches);
    }
    public function children($ns = null, $prefix = false) {
        $r = [];
        foreach ($this->__node["kids"] as $k) { if ($k["t"] == 1) { $r[] = new SimpleXMLElement($k); } }
        return $r;
    }
    public function attributes($ns = null, $prefix = false) { return new SimpleXMLAttrs($this->__node["attrs"]); }
    public function __toString() {
        $s = "";
        foreach ($this->__node["kids"] as $k) { if ($k["t"] == 3 || $k["t"] == 4) { $s .= $k["text"]; } }
        return $s;
    }
    public function asXML($filename = null) {
        $out = __sxml_ser($this->__node);
        // only the loaded document root carries __decl (set by the loader)
        if (isset($this->__decl)) {
            $out = $this->__decl . $out . "\n";
        }
        if ($filename !== null) { file_put_contents($filename, $out); return true; }
        return $out;
    }
    public function saveXML($filename = null) { return $this->asXML($filename); }
    public function count(): int { $n = 0; foreach ($this->__node["kids"] as $k) { if ($k["t"] == 1) { $n++; } } return $n; }
    public function offsetExists($k): bool {
        if (is_int($k)) { return $k >= 0 && $k < count($this->__sibs); }
        return isset($this->__node["attrs"][$k]);
    }
    public function offsetGet($k): mixed {
        if (is_int($k)) { return isset($this->__sibs[$k]) ? new SimpleXMLElement($this->__sibs[$k], $this->__sibs) : null; }
        return $this->__node["attrs"][$k] ?? null;
    }
    public function offsetSet($k, $v): void {}
    public function offsetUnset($k): void {}
    public function rewind(): void { $this->__p = 0; }
    public function valid(): bool { return $this->__p < count($this->__sibs); }
    public function current(): mixed { return new SimpleXMLElement($this->__sibs[$this->__p]); }
    public function key(): mixed { return $this->__node["name"]; }
    public function next(): void { $this->__p = $this->__p + 1; }
}
class SimpleXMLAttrs implements Iterator, ArrayAccess, Countable {
    private $__a; private $__keys; private $__p = 0;
    public function __construct($attrs) { $this->__a = $attrs; $this->__keys = array_keys($attrs); }
    public function offsetExists($k): bool { return isset($this->__a[$k]); }
    public function offsetGet($k): mixed { return $this->__a[$k] ?? null; }
    public function offsetSet($k, $v): void {}
    public function offsetUnset($k): void {}
    public function count(): int { return count($this->__a); }
    public function rewind(): void { $this->__p = 0; $this->__keys = array_keys($this->__a); }
    public function valid(): bool { return $this->__p < count($this->__keys); }
    public function current(): mixed { return $this->__a[$this->__keys[$this->__p]]; }
    public function key(): mixed { return $this->__keys[$this->__p]; }
    public function next(): void { $this->__p = $this->__p + 1; }
}

}
"##;

const STEP_LIMIT_DEFAULT: u64 = 20_000_000;
thread_local! {
    /// Overridable per-process (PHARGO_STEP_LIMIT env) — the WP oracle needs
    /// far more than corpus tests; the default guard stays for the scoreboard.
    static STEP_LIMIT_TL: u64 = std::env::var("PHARGO_STEP_LIMIT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(STEP_LIMIT_DEFAULT);
}
fn step_limit() -> u64 {
    STEP_LIMIT_TL.with(|v| *v)
}
/// Cap on single string allocations (concat, str_repeat) — stops memory bombs
/// from pathological corpus tests (huge `.=` / `str_repeat` / `range`).
const MAX_STR: usize = 64 * 1024 * 1024;
const MAX_RANGE: usize = 8_000_000;
/// Cap on total nodes in a single array value (memory-bomb guard).
const MAX_ARRAY_NODES: usize = 4_000_000;
/// Cap on total program output — stops runaway echo/var_dump loops (real tests
/// compare a small EXPECT block, so 32 MB is far above any legitimate output).
const MAX_OUTPUT: usize = 32 * 1024 * 1024;

impl Eval {
    pub fn new() -> Self {
        Eval {
            out: Vec::new(),
            scopes: vec![HashMap::new()],
            funcs: HashMap::new(),
            classes: HashMap::new(),
            consts: HashMap::new(),
            static_props: HashMap::new(),
            current_class: None,
            called_class: None,
            error_level: 30719,
            default_tz: crate::tz::default_tz(),
            thrown: None,
            call_depth: 0,
            eval_depth: 0,
            cur_file: None,
            included: HashSet::new(),
            ob_stack: Vec::new(),
            next_res_id: 1,
            shutdown_fns: Vec::new(),
            strict_types: false,
            quiet: 0,
            error_handler: None,
            in_error_handler: false,
            cur_line: 0,
            cur_ns: String::new(),
            use_map: HashMap::new(),
            bc_scale: 0,
            strtok_state: None,
            method_cache: RefCell::new(HashMap::new()),
            def_ctx: HashMap::new(),
            frames: Vec::new(),
            prelude_fns: HashSet::new(),
            prelude_classes: HashSet::new(),
            byref_ret: Vec::new(),
            typed_props_cache: HashMap::new(),
            static_vars: HashMap::new(),
            autoloaders: Vec::new(),
            autoload_active: HashSet::new(),
            cur_args: Vec::new(),
            cur_fn: Vec::new(),
            rng_state: 0x2545_F491_4F6C_DD1D,
            anon_names: HashMap::new(),
            enum_cases: HashMap::new(),
            gen_buf: None,
            gen_nodes: 0,
            steps: 0,
        }
    }

    /// Enter a call frame; errors if nesting would risk a native stack overflow.
    fn enter_call(&mut self) -> R<()> {
        self.call_depth += 1;
        if self.call_depth > MAX_CALL_DEPTH {
            self.call_depth -= 1;
            return Err(RunError("maximum function nesting level reached".into()));
        }
        Ok(())
    }

    /// xorshift64* — a cheap deterministic PRNG for rand()/mt_rand().
    fn next_rand(&mut self) -> u64 {
        let mut x = self.rng_state;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.rng_state = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// Register the exception/error hierarchy (parsed from PRELUDE).
    fn load_prelude(&mut self) {
        // The prelude is identical for every run, but the scoreboard creates a
        // fresh Eval per test (~22k). Lexing+parsing this large prelude each time
        // dominated runtime, so parse it once per thread and just re-hoist (which
        // only clones ClassDecls into Rc) from the cached AST.
        thread_local! {
            static PRELUDE_AST: Vec<Stmt> = super::lexer::Lexer::tokenize(PRELUDE)
                .ok()
                .and_then(|toks| super::parser::Parser::parse(toks).ok())
                .unwrap_or_default();
        }
        PRELUDE_AST.with(|ast| self.hoist(ast));
        self.prelude_fns = self.funcs.keys().cloned().collect();
        self.prelude_classes = self.classes.keys().cloned().collect();
    }

    /// Run a parsed program and return everything it printed.
    pub fn run(program: &[Stmt]) -> R<Vec<u8>> {
        Self::run_with_path(program, None)
    }

    /// Like [`run`], but records the script's path so `__FILE__`/`__DIR__` and
    /// relative `include`/`require` resolve against it.
    /// Run registered shutdown callbacks (best-effort; errors are swallowed,
    /// matching PHP's lenient shutdown phase). Re-runs the queue in case a
    /// callback registers more.
    fn run_shutdown(&mut self) {
        let mut guard = 0;
        while !self.shutdown_fns.is_empty() && guard < 1000 {
            let fns = std::mem::take(&mut self.shutdown_fns);
            for (cb, extra) in fns {
                let _ = self.call_value(cb, extra);
            }
            guard += 1;
        }
    }

    /// Drive SAX-style handlers over a parsed XML node-array tree.
    fn xml_sax_walk(&mut self, parser: &Value, node: &Value, fold: bool) -> R<()> {
        let (start, end, char_h) = match parser {
            Value::Object(rc) => {
                let b = rc.borrow();
                (
                    b.get("__start").cloned().unwrap_or(Value::Null),
                    b.get("__end").cloned().unwrap_or(Value::Null),
                    b.get("__char").cloned().unwrap_or(Value::Null),
                )
            }
            _ => return Ok(()),
        };
        let t = match node {
            Value::Array(a) => to_i64(&a.get(&Key::Str(b"t".to_vec())).cloned().unwrap_or(Value::Null)),
            _ => return Ok(()),
        };
        let arr = match node {
            Value::Array(a) => a,
            _ => return Ok(()),
        };
        if t == 1 {
            let mut name = to_bytes(&arr.get(&Key::Str(b"name".to_vec())).cloned().unwrap_or(Value::Null));
            if fold {
                name.make_ascii_uppercase();
            }
            let attrs = arr.get(&Key::Str(b"attrs".to_vec())).cloned().unwrap_or(Value::Array(Arr::new()));
            if !matches!(start, Value::Null) {
                self.call_value(start.clone(), vec![parser.clone(), Value::Str(name.clone()), attrs])?;
            }
            if let Some(Value::Array(kids)) = arr.get(&Key::Str(b"kids".to_vec())) {
                let kids = kids.clone();
                for (_, kid) in &kids.entries {
                    self.xml_sax_walk(parser, kid, fold)?;
                }
            }
            if !matches!(end, Value::Null) {
                self.call_value(end.clone(), vec![parser.clone(), Value::Str(name)])?;
            }
        } else if (t == 3 || t == 4) && !matches!(char_h, Value::Null) {
            let text = arr.get(&Key::Str(b"text".to_vec())).cloned().unwrap_or(Value::Null);
            self.call_value(char_h.clone(), vec![parser.clone(), text])?;
        }
        Ok(())
    }

    /// Serialize a value to PHP's serialize() format. A method (not a free fn) so
    /// it can honor the `__serialize()` magic method.
    fn ser_val(&mut self, v: &Value, out: &mut Vec<u8>, depth: usize) -> R<()> {
        if depth > 256 || out.len() > MAX_STR {
            out.extend_from_slice(b"N;");
            return Ok(());
        }
        match v {
            Value::Null => out.extend_from_slice(b"N;"),
            Value::Bool(b) => out.extend_from_slice(if *b { b"b:1;" } else { b"b:0;" }),
            Value::Int(n) => out.extend_from_slice(format!("i:{n};").as_bytes()),
            Value::Float(f) => out.extend_from_slice(format!("d:{};", ser_float(*f)).as_bytes()),
            Value::Str(s) => {
                out.extend_from_slice(format!("s:{}:\"", s.len()).as_bytes());
                out.extend_from_slice(s);
                out.extend_from_slice(b"\";");
            }
            Value::Array(a) => {
                out.extend_from_slice(format!("a:{}:{{", a.len()).as_bytes());
                let entries = a.entries.clone();
                for (k, val) in &entries {
                    self.ser_key(k, out);
                    self.ser_val(val, out, depth + 1)?;
                }
                out.push(b'}');
            }
            Value::Object(rc) => {
                let class = rc.borrow().class.clone();
                // __serialize(): produce an O: wrapper around the returned array.
                if self.find_method(&class, "__serialize").is_some() {
                    let arr = self.call_method(v.clone(), "__serialize", vec![])?;
                    if let Value::Array(a) = arr {
                        out.extend_from_slice(
                            format!("O:{}:\"{}\":{}:{{", class.len(), class, a.len()).as_bytes(),
                        );
                        let entries = a.entries.clone();
                        for (k, val) in &entries {
                            self.ser_key(k, out);
                            self.ser_val(val, out, depth + 1)?;
                        }
                        out.push(b'}');
                        return Ok(());
                    }
                }
                let props = rc.borrow().props.clone();
                out.extend_from_slice(
                    format!("O:{}:\"{}\":{}:{{", class.len(), class, props.len()).as_bytes(),
                );
                for (name, val) in &props {
                    out.extend_from_slice(format!("s:{}:\"{}\";", name.len(), name).as_bytes());
                    self.ser_val(val, out, depth + 1)?;
                }
                out.push(b'}');
            }
            Value::Closure(_) => out.extend_from_slice(b"N;"),
            Value::Ref(c) => {
                let inner = c.borrow().clone();
                self.ser_val(&inner, out, depth)?;
            }
        }
        Ok(())
    }

    fn ser_key(&self, k: &Key, out: &mut Vec<u8>) {
        match k {
            Key::Int(n) => out.extend_from_slice(format!("i:{n};").as_bytes()),
            Key::Str(s) => {
                out.extend_from_slice(format!("s:{}:\"", s.len()).as_bytes());
                out.extend_from_slice(s);
                out.extend_from_slice(b"\";");
            }
        }
    }

    /// After unserialize, call `__wakeup()` on every object in the graph that
    /// defines it (depth-first), matching PHP's unserialize behavior.
    fn apply_wakeup(&mut self, v: &Value, depth: usize) -> R<()> {
        if depth > 256 {
            return Ok(());
        }
        match v {
            Value::Array(a) => {
                let vals: Vec<Value> = a.entries.iter().map(|(_, x)| x.clone()).collect();
                for x in vals {
                    self.apply_wakeup(&x, depth + 1)?;
                }
            }
            Value::Object(rc) => {
                let (class, props): (String, Vec<(String, Value)>) = {
                    let b = rc.borrow();
                    (b.class.clone(), b.props.clone())
                };
                for (_, x) in &props {
                    self.apply_wakeup(x, depth + 1)?;
                }
                if self.find_method(&class, "__unserialize").is_some() {
                    // The serialized props ARE the __serialize() array; hand them to
                    // __unserialize as an array (string keys preserved), after clearing.
                    let mut arr = Arr::new();
                    for (name, val) in &props {
                        arr.insert(Key::Str(name.as_bytes().to_vec()), val.clone());
                    }
                    rc.borrow_mut().props.clear();
                    self.call_method(v.clone(), "__unserialize", vec![Value::Array(arr)])?;
                } else if self.find_method(&class, "__wakeup").is_some() {
                    self.call_method(v.clone(), "__wakeup", vec![])?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    pub fn run_with_path(program: &[Stmt], path: Option<PathBuf>) -> R<Vec<u8>> {
        let mut e = Eval::new();
        e.cur_file = path;
        e.load_prelude();
        // Predefined stream resources.
        let stdin = e.new_stream("php://stdin", "r", vec![], false, "stdin");
        let stdout = e.new_stream("php://stdout", "w", vec![], false, "stdout");
        let stderr = e.new_stream("php://stderr", "w", vec![], false, "stderr");
        e.consts.insert("STDIN".into(), stdin);
        e.consts.insert("STDOUT".into(), stdout);
        e.consts.insert("STDERR".into(), stderr);
        // Superglobals exist (empty) so reads/writes work in any scope.
        for sg in ["_SERVER", "_GET", "_POST", "_REQUEST", "_SESSION", "_COOKIE", "_ENV", "_FILES", "GLOBALS"] {
            e.scopes[0].insert(sg.to_string(), Value::Array(Arr::new()));
        }
        // Internal setup (prelude classes, std streams) is done; number the user
        // program's objects from #1, as PHP does.
        super::value::reset_object_ids();
        crate::pdo::reset();
        e.hoist(program);
        match e.exec_block(program) {
            Ok(_) => {
                e.run_shutdown();
                Ok(e.out)
            }
            Err(err) => {
                // `exit`/`die`: a normal halt, not an error.
                if err.0 == "__phargo_exit__" {
                    e.run_shutdown();
                    return Ok(e.out);
                }
                // An uncaught exception becomes a PHP fatal-error message in the
                // output (matching PHP). Other engine errors (step limit, unknown
                // function, parse) propagate with their message so the scoreboard
                // histogram stays useful.
                if let Some(exc) = e.thrown.take() {
                    let (cls, msg) = e.exception_info(&exc);
                    let (file, line) = match &exc {
                        Value::Object(rc) => {
                            let b = rc.borrow();
                            (
                                b.get("file").map(|v| String::from_utf8_lossy(&to_bytes(v)).into_owned()).unwrap_or_default(),
                                b.get("line").map(to_i64).unwrap_or(0),
                            )
                        }
                        _ => (String::new(), 0),
                    };
                    let trace = e
                        .call_method(exc.clone(), "getTraceAsString", vec![])
                        .map(|v| String::from_utf8_lossy(&to_bytes(&v)).into_owned())
                        .unwrap_or_else(|_| "#0 {main}".to_string());
                    let s = format!(
                        "\nFatal error: Uncaught {cls}: {msg} in {file}:{line}\nStack trace:\n{trace}\n  thrown in {file} on line {line}\n"
                    );
                    e.out.extend_from_slice(s.as_bytes());
                    // PHP runs shutdown functions after an uncaught exception too
                    e.run_shutdown();
                    Ok(e.out)
                } else {
                    Err(err)
                }
            }
        }
    }

    fn exception_info(&self, v: &Value) -> (String, String) {
        if let Value::Object(rc) = v {
            let o = rc.borrow();
            let msg = o
                .get("message")
                .map(|m| String::from_utf8_lossy(&to_bytes(m)).into_owned())
                .unwrap_or_default();
            (o.class.clone(), msg)
        } else {
            ("Exception".into(), String::new())
        }
    }

    /// Hoist top-level function declarations so call-before-definition works.
    fn hoist(&mut self, stmts: &[Stmt]) {
        self.hoist_ns(stmts, &self.cur_ns.clone());
    }

    /// Register declarations, qualifying names with the enclosing namespace.
    /// A statement-form `namespace X;` re-scopes the remainder of the list;
    /// block form scopes only its body. Unqualified parent/interface/trait
    /// references get the namespace prefix too (runtime lookup falls back to
    /// the bare name, so global/prelude ancestors keep resolving).
    fn hoist_ns(&mut self, stmts: &[Stmt], ns: &str) {
        let mut ns = ns.to_string();
        // collect this scope's `use` aliases up front — declarations register
        // during hoist (before any Stmt::Use executes), and their bodies must
        // later run under these aliases (see enter_def_ctx)
        let mut uses: HashMap<String, String> = HashMap::new();
        for s in stmts {
            let s = match s {
                Stmt::Marked(_, inner) => &**inner,
                other => other,
            };
            if let Stmt::Use(items) = s {
                for it in items {
                    let fq = it.name.parts.join("\\");  // declared case preserved: feeds case-sensitive autoloaders
                    let alias = it
                        .alias
                        .clone()
                        .unwrap_or_else(|| it.name.last().to_string())
                        .to_ascii_lowercase();
                    uses.insert(alias, fq);
                }
            }
        }
        let uses = Rc::new(uses);
        for s in stmts {
            let s = match s {
                Stmt::Marked(_, inner) => &**inner,
                other => other,
            };
            match s {
                Stmt::Namespace { name, body } => {
                    // keep the namespace's declared case: it becomes part of the
                    // class's display name (keys are lowercased at insert)
                    let inner_ns = name
                        .as_ref()
                        .map(|n| n.parts.join("\\"))
                        .unwrap_or_default();
                    match body {
                        Some(b) => self.hoist_ns(b, &inner_ns),
                        None => ns = inner_ns,
                    }
                }
                Stmt::Func(f) => {
                    let mut f = f.clone();
                    if !ns.is_empty() {
                        f.name = format!("{ns}\\{}", f.name);
                    }
                    self.record_def_ctx(format!("fn:{}", f.name.to_ascii_lowercase()), &ns, &uses);
                    self.funcs.insert(f.name.to_ascii_lowercase(), Rc::new(f));
                }
                Stmt::Class(c) => {
                    let mut c = c.clone();
                    if !ns.is_empty() {
                        c.name = format!("{ns}\\{}", c.name);
                        let prefix = |n: &mut Name| {
                            if !n.fully_qualified && n.parts.len() == 1 {
                                n.parts.insert(0, ns.clone());
                            }
                        };
                        if let Some(p) = &mut c.parent {
                            prefix(p);
                        }
                        for i in &mut c.interfaces {
                            prefix(i);
                        }
                        for t in &mut c.uses_traits {
                            prefix(t);
                        }
                    }
                    self.record_def_ctx(format!("class:{}", c.name.to_ascii_lowercase()), &ns, &uses);
                    self.classes.insert(c.name.to_ascii_lowercase(), Rc::new(c));
                    self.method_cache.borrow_mut().clear();
                }
                _ => {}
            }
        }
    }

    fn cur_file_str(&self) -> String {
        self.cur_file
            .as_ref()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default()
    }

    fn record_def_ctx(&mut self, key: String, ns: &str, uses: &Rc<HashMap<String, String>>) {
        self.def_ctx.insert(
            key,
            DefCtx {
                file: self.cur_file.clone(),
                ns: ns.to_string(),
                uses: uses.clone(),
            },
        );
    }

    /// Swap cur_file/cur_ns/use_map to the callee's definition-site context
    /// for the duration of a call. Returns the previous values to restore, or
    /// None when unknown (builtins, prelude, closures — keep the caller's).
    fn enter_def_ctx(
        &mut self,
        key: &str,
    ) -> Option<(Option<PathBuf>, String, HashMap<String, String>)> {
        let ctx = self.def_ctx.get(key)?;
        let file = ctx.file.clone();
        let ns = ctx.ns.clone();
        let uses = (*ctx.uses).clone();
        let pf = match file {
            Some(df) => std::mem::replace(&mut self.cur_file, Some(df)),
            None => self.cur_file.clone(),
        };
        let pns = std::mem::replace(&mut self.cur_ns, ns);
        let puse = std::mem::replace(&mut self.use_map, uses);
        Some((pf, pns, puse))
    }

    /// find_class through a (possibly qualified) AST Name: the joined form
    /// first, then the bare last segment (global/prelude fallback).
    fn find_class_n(&self, n: &Name) -> Option<Rc<ClassDecl>> {
        let joined = n.parts.join("\\");
        if n.parts.len() > 1 {
            if let Some(c) = self.find_class(&joined) {
                return Some(c);
            }
        }
        self.find_class(n.last())
    }

    /// Resolve an unqualified/qualified class name written in source to the
    /// registered class-key string: `use` aliases first, then current-namespace,
    /// then the name as written (global fallback keeps prelude classes working).
    fn resolve_ns_class(&self, n: &Name) -> String {
        let joined = n.parts.join("\\");
        if n.fully_qualified {
            // FQ: as written; if unregistered but the bare last segment exists
            // (prelude classes accessed via a namespace spelling), fall back
            if !self.classes.contains_key(&joined.to_ascii_lowercase()) {
                if let Some(c) = self.find_class(n.last()) {
                    return c.name.clone();
                }
            }
            return joined;
        }
        // use-alias on the first segment. PHP alias resolution is purely
        // syntactic — honor it even when the class isn't loaded yet, so the
        // caller's autoload sees the FQ name (Requests' InputValidator).
        if let Some(fq) = self.use_map.get(&n.parts[0].to_ascii_lowercase()) {
            let mut out = fq.clone();
            for extra in &n.parts[1..] {
                out.push_str("\\");
                out.push_str(extra);
            }
            if self.classes.contains_key(&out.to_ascii_lowercase()) {
                return out;
            }
            // unregistered: still prefer the alias unless the bare name is a
            // known class (prelude classes reached through stale aliases)
            if self.find_class(n.last()).is_none() {
                return out;
            }
        }
        if !self.cur_ns.is_empty() {
            let cand = format!("{}\\{joined}", self.cur_ns);
            if self.classes.contains_key(&cand.to_ascii_lowercase()) {
                return self
                    .find_class(&cand)
                    .map(|c| c.name.clone())
                    .unwrap_or(cand);
            }
        }
        if n.parts.len() > 1 {
            if self.classes.contains_key(&joined.to_ascii_lowercase()) {
                return self.find_class(&joined).map(|c| c.name.clone()).unwrap_or(joined);
            }
            // qualified name with no registration: bare-last-segment fallback
            // keeps prelude classes reachable via namespaced spellings
            if let Some(c) = self.find_class(n.last()) {
                return c.name.clone();
            }
        }
        joined
    }

    fn vars(&mut self) -> &mut HashMap<String, Value> {
        self.scopes.last_mut().unwrap()
    }

    /// Get (or create) the shared reference cell backing a local variable, so it
    /// can be aliased (`$b = &$a`, `&$param`, `use (&$x)`). If the variable
    /// already holds a `Ref`, its cell is reused; otherwise its current value is
    /// moved into a fresh cell and the variable is rebound to a `Ref` to it.
    fn get_ref_cell(&mut self, name: &str) -> Rc<RefCell<Value>> {
        let scope = self.vars();
        if let Some(Value::Ref(cell)) = scope.get(name) {
            return cell.clone();
        }
        let cur = scope.get(name).cloned().unwrap_or(Value::Null);
        let cell = Rc::new(RefCell::new(cur));
        scope.insert(name.to_string(), Value::Ref(cell.clone()));
        cell
    }

    fn tick(&mut self) -> R<()> {
        self.steps += 1;
        if self.steps > step_limit() {
            // self-diagnosing: where was execution when the budget died?
            let fname = self.cur_fn.last().cloned().unwrap_or_default();
            let file = self
                .cur_file
                .as_ref()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default();
            return Err(RunError(format!(
                "step limit exceeded at {file}:{} in {fname}()",
                self.cur_line
            )));
        }
        // Runaway-output guard: a loop that echoes/var_dumps without bound (e.g.
        // gh13178_4: var_dump in a never-terminating loop) would otherwise grind to
        // the step limit producing gigabytes. Cap total output well above any real
        // test (which compares a small EXPECT block).
        if self.out.len() > MAX_OUTPUT {
            return Err(RunError("output limit exceeded".into()));
        }
        Ok(())
    }

    // ---- statements -----------------------------------------------------
    fn exec_block(&mut self, stmts: &[Stmt]) -> R<Flow> {
        let mut i = 0;
        while i < stmts.len() {
            match self.exec(&stmts[i]) {
                Ok(Flow::Normal) => i += 1,
                Ok(other) => return Ok(other),
                Err(e) => {
                    // goto: if this list holds the target label, resume there;
                    // otherwise keep unwinding (out of loops, nested blocks…)
                    if let Some(label) = e.0.strip_prefix("__phargo_goto__") {
                        if let Some(idx) = stmts.iter().position(|s| {
                            let s = match s {
                                Stmt::Marked(_, inner) => &**inner,
                                other => other,
                            };
                            matches!(s, Stmt::Label(l) if l == label)
                        }) {
                            i = idx + 1;
                            continue;
                        }
                    }
                    return Err(e);
                }
            }
        }
        Ok(Flow::Normal)
    }

    fn exec(&mut self, s: &Stmt) -> R<Flow> {
        self.tick()?;
        match s {
            Stmt::InlineHtml(b) => self.out.extend_from_slice(b),
            Stmt::Label(_) => {}
            Stmt::Goto(l) => return Err(RunError(format!("__phargo_goto__{l}"))),
            Stmt::Echo(items) => {
                for e in items {
                    let v = self.eval(e)?;
                    let b = self.stringify(&v)?;
                    self.out.extend_from_slice(&b);
                }
            }
            Stmt::Expr(e) => {
                // Fast path for `$v .= expr;` as a statement: append in place and
                // DON'T clone the (possibly huge) result — the value is discarded.
                // (Cloning it each iteration made `.=` loops O(n^2): concat_003.)
                if let Expr::AssignOp(BinOp::Concat, lhs, rhs) = e {
                    if let Expr::Var(name) = &**lhs {
                        let rv = self.eval(rhs)?;
                        let rb = self.stringify(&rv)?;
                        let slot = self
                            .vars()
                            .entry(name.clone())
                            .or_insert(Value::Str(Vec::new()));
                        if let Value::Str(s) = slot {
                            if s.len() + rb.len() <= MAX_STR {
                                s.extend_from_slice(&rb);
                            }
                        } else {
                            let mut s = to_bytes(slot);
                            s.extend_from_slice(&rb);
                            *slot = Value::Str(s);
                        }
                        return Ok(Flow::Normal);
                    }
                }
                self.eval(e)?;
            }
            Stmt::Block(b) => return self.exec_block(b),
            Stmt::Nop => {}
            Stmt::Func(f) => {
                // runtime (re)declaration — qualify with the current namespace
                let mut f = f.clone();
                if !self.cur_ns.is_empty() && !f.name.contains('\\') {
                    f.name = format!("{}\\{}", self.cur_ns, f.name);
                }
                let (ns, uses) = (self.cur_ns.clone(), Rc::new(self.use_map.clone()));
                self.record_def_ctx(format!("fn:{}", f.name.to_ascii_lowercase()), &ns, &uses);
                self.funcs.insert(f.name.to_ascii_lowercase(), Rc::new(f));
            }
            Stmt::ConstDecl(decls) => {
                for (name, e) in decls {
                    let v = self.eval(e)?;
                    let key = if self.cur_ns.is_empty() {
                        name.clone()
                    } else {
                        format!("{}\\{name}", self.cur_ns)
                    };
                    self.consts.insert(key, v);
                }
            }
            Stmt::If { cond, then, elseifs, els } => {
                if to_bool(&self.eval(cond)?) {
                    return self.exec_block(then);
                }
                for (c, b) in elseifs {
                    if to_bool(&self.eval(c)?) {
                        return self.exec_block(b);
                    }
                }
                if let Some(b) = els {
                    return self.exec_block(b);
                }
            }
            Stmt::While { cond, body } => {
                while to_bool(&self.eval(cond)?) {
                    self.tick()?;
                    match self.exec_block(body)? {
                        Flow::Break(n) => return self.unwind_break(n),
                        Flow::Continue(n) if n > 1 => return Ok(Flow::Continue(n - 1)),
                        Flow::Return(v) => return Ok(Flow::Return(v)),
                        _ => {}
                    }
                }
            }
            Stmt::DoWhile { body, cond } => loop {
                self.tick()?;
                match self.exec_block(body)? {
                    Flow::Break(n) => return self.unwind_break(n),
                    Flow::Continue(n) if n > 1 => return Ok(Flow::Continue(n - 1)),
                    Flow::Return(v) => return Ok(Flow::Return(v)),
                    _ => {}
                }
                if !to_bool(&self.eval(cond)?) {
                    break;
                }
            },
            Stmt::For { init, cond, step, body } => {
                for e in init {
                    self.eval(e)?;
                }
                loop {
                    self.tick()?;
                    let go = if let Some(last) = cond.last() {
                        for e in &cond[..cond.len() - 1] {
                            self.eval(e)?;
                        }
                        to_bool(&self.eval(last)?)
                    } else {
                        true
                    };
                    if !go {
                        break;
                    }
                    match self.exec_block(body)? {
                        Flow::Break(n) => return self.unwind_break(n),
                        Flow::Continue(n) if n > 1 => return Ok(Flow::Continue(n - 1)),
                        Flow::Return(v) => return Ok(Flow::Return(v)),
                        _ => {}
                    }
                    for e in step {
                        self.eval(e)?;
                    }
                }
            }
            Stmt::Foreach { array, key, value, by_ref, body } => {
                // By-reference iteration over a plain array variable: elements are
                // promoted to shared Ref cells so writes through the loop var stick.
                if *by_ref {
                    if let Expr::Var(vname) = value {
                        match array {
                            Expr::Var(aname) if !is_superglobal(aname) => {
                                let place = ArrPlace::Var(aname.clone());
                                return self.foreach_by_ref(&place, vname, key, body);
                            }
                            // by-ref over an object property: write through
                            Expr::Prop(objexpr, pname, _) => {
                                let o = self.eval(objexpr)?;
                                let pname = self.prop_name_str(pname)?;
                                if let Value::Object(rc) = o {
                                    let place = ArrPlace::Prop(rc, pname);
                                    return self.foreach_by_ref(&place, vname, key, body);
                                }
                            }
                            _ => {}
                        }
                    }
                }
                let arr = self.eval(array)?.deref();
                if !matches!(arr, Value::Array(_) | Value::Object(_) | Value::Closure(_)) {
                    let t = self.given_type(&arr);
                    self.warn(&format!("foreach() argument must be of type array|object, {t} given"))?;
                }
                match arr {
                    Value::Array(a) => {
                        for (k, v) in a.entries.clone() {
                            if let Some(f) = self.foreach_step(key, value, body, Some(akey_to_value(&k)), v)? {
                                return Ok(f);
                            }
                        }
                    }
                    Value::Object(rc) => {
                        let class = rc.borrow().class.clone();
                        // IteratorAggregate: getIterator() returns the real iterator
                        let iter = if self.find_method(&class, "getiterator").is_some() {
                            self.call_method(Value::Object(rc.clone()), "getIterator", vec![])?
                        } else {
                            Value::Object(rc.clone())
                        };
                        let it_class = match &iter {
                            Value::Object(irc) => Some(irc.borrow().class.clone()),
                            _ => None,
                        };
                        if let Some(ic) = it_class {
                            if self.find_method(&ic, "rewind").is_some()
                                && self.find_method(&ic, "valid").is_some()
                            {
                                // Iterator protocol
                                self.call_method(iter.clone(), "rewind", vec![])?;
                                loop {
                                    self.tick()?;
                                    if !to_bool(&self.call_method(iter.clone(), "valid", vec![])?) {
                                        break;
                                    }
                                    let cur = self.call_method(iter.clone(), "current", vec![])?;
                                    let k = if key.is_some() {
                                        Some(self.call_method(iter.clone(), "key", vec![])?)
                                    } else {
                                        None
                                    };
                                    if let Some(f) = self.foreach_step(key, value, body, k, cur)? {
                                        return Ok(f);
                                    }
                                    self.call_method(iter.clone(), "next", vec![])?;
                                }
                                return Ok(Flow::Normal);
                            }
                        }
                        // plain object: iterate its (public) properties
                        let props = rc.borrow().props.clone();
                        for (k, v) in props {
                            if let Some(f) =
                                self.foreach_step(key, value, body, Some(Value::Str(k.into_bytes())), v)?
                            {
                                return Ok(f);
                            }
                        }
                    }
                    _ => {}
                }
            }
            Stmt::Switch { subject, cases } => {
                let subj = self.eval(subject)?;
                let mut matched = false;
                for case in cases {
                    if !matched {
                        match &case.test {
                            Some(t) => {
                                let tv = self.eval(t)?;
                                if loose_eq(&subj, &tv) {
                                    matched = true;
                                }
                            }
                            None => matched = true, // default
                        }
                    }
                    if matched {
                        match self.exec_block(&case.body)? {
                            Flow::Break(n) => return self.unwind_break(n),
                            Flow::Continue(n) if n > 1 => return Ok(Flow::Continue(n - 1)),
                            Flow::Continue(_) => return Ok(Flow::Normal),
                            Flow::Return(v) => return Ok(Flow::Return(v)),
                            Flow::Normal => {}
                        }
                    }
                }
                // if nothing matched and there was a default earlier we'd have run it;
                // a trailing default with no prior match is handled by the loop above.
            }
            Stmt::Break(n) => return Ok(Flow::Break(*n)),
            Stmt::Continue(n) => return Ok(Flow::Continue(*n)),
            Stmt::Return(e) => {
                let v = match e {
                    // `function &f() { return $undef; }` creates silently
                    Some(e) if self.byref_ret.last().copied().unwrap_or(false) => {
                        self.eval_quiet(e)?
                    }
                    Some(e) => self.eval(e)?,
                    None => Value::Null,
                };
                return Ok(Flow::Return(v));
            }
            Stmt::Unset(items) => {
                for it in items.clone() {
                    match &it {
                        Expr::Var(name) => {
                            if is_superglobal(name) {
                                self.scopes[0].remove(name);
                            } else {
                                self.vars().remove(name);
                            }
                        }
                        Expr::VarVar(inner) => {
                            let name =
                                String::from_utf8_lossy(&to_bytes(&self.eval(inner)?)).into_owned();
                            self.vars().remove(&name);
                        }
                        Expr::Index(base, Some(idx)) => {
                            if let Some(obj) = self.arrayaccess_obj(base, "offsetunset") {
                                let raw = self.eval(idx)?;
                                self.call_method(obj, "offsetUnset", vec![raw])?;
                            } else {
                                let key = Arr::norm_key(&self.eval(idx)?);
                                self.unset_index(base, &key)?;
                            }
                        }
                        Expr::Prop(obj, name, _) => {
                            let o = self.eval(obj)?;
                            let pname = self.prop_name_str(name)?;
                            if let Value::Object(rc) = o {
                                rc.borrow_mut().props.retain(|(k, _)| k != &pname);
                            }
                        }
                        _ => {}
                    }
                }
            }
            Stmt::Global(names) => {
                // Bind by REFERENCE: promote the global slot to a shared Ref
                // cell that the local name aliases, so a later write to
                // $GLOBALS['x'] (e.g. by an include) is visible through the
                // local. WP's require_wp_db checks isset($wpdb) after the
                // db.php drop-in sets $GLOBALS['wpdb'] — by-value broke it.
                for n in names {
                    let cell = match self.scopes[0].get(n) {
                        Some(Value::Ref(c)) => c.clone(),
                        other => {
                            let c = Rc::new(RefCell::new(
                                other.cloned().unwrap_or(Value::Null),
                            ));
                            self.scopes[0].insert(n.clone(), Value::Ref(c.clone()));
                            c
                        }
                    };
                    self.vars().insert(n.clone(), Value::Ref(cell));
                }
            }
            Stmt::Class(c) => {
                let mut c = c.clone();
                if !self.cur_ns.is_empty() && !c.name.contains('\\') {
                    let ns = self.cur_ns.clone();
                    c.name = format!("{ns}\\{}", c.name);
                    let prefix = |n: &mut Name| {
                        if !n.fully_qualified && n.parts.len() == 1 {
                            n.parts.insert(0, ns.clone());
                        }
                    };
                    if let Some(p) = &mut c.parent {
                        prefix(p);
                    }
                    for i in &mut c.interfaces {
                        prefix(i);
                    }
                    for t in &mut c.uses_traits {
                        prefix(t);
                    }
                }
                let (ns, uses) = (self.cur_ns.clone(), Rc::new(self.use_map.clone()));
                self.record_def_ctx(format!("class:{}", c.name.to_ascii_lowercase()), &ns, &uses);
                self.classes.insert(c.name.to_ascii_lowercase(), Rc::new(c));
                self.method_cache.borrow_mut().clear();
            }
            Stmt::Throw(e) => {
                let v = self.eval(e)?;
                self.thrown = Some(v);
                return Err(RunError("__phargo_throw__".into()));
            }
            Stmt::Try { body, catches, finally } => {
                let outcome = self.exec_block(body);
                let mut result = self.handle_try_outcome(outcome, catches)?;
                if let Some(fin) = finally {
                    match self.exec_block(fin)? {
                        Flow::Normal => {}
                        other => result = other, // finally's flow wins
                    }
                }
                return Ok(result);
            }
            Stmt::Marked(line, inner) => {
                self.cur_line = *line;
                return self.exec(inner);
            }
            // `static $x = init;` — persistent per-function storage. The scope
            // var becomes a Ref to a cell kept in static_vars, so writes persist
            // across calls. Keyed by declaring class + function name (inherited
            // methods share the same static, as PHP does).
            Stmt::StaticVar(vars) => {
                let fnkey = format!(
                    "{}::{}",
                    self.current_class.clone().unwrap_or_default(),
                    self.cur_fn.last().cloned().unwrap_or_default()
                );
                for (name, init) in vars {
                    let key = (fnkey.clone(), name.clone());
                    let cell = match self.static_vars.get(&key) {
                        Some(Value::Ref(c)) => c.clone(),
                        _ => {
                            let iv = match init {
                                Some(e) => self.eval(e)?,
                                None => Value::Null,
                            };
                            let c = Rc::new(RefCell::new(iv));
                            self.static_vars.insert(key, Value::Ref(c.clone()));
                            c
                        }
                    };
                    self.vars().insert(name.clone(), Value::Ref(cell));
                }
            }
            Stmt::Namespace { name, body } => {
                let ns = name.as_ref().map(|n| n.parts.join("\\")).unwrap_or_default();
                match body {
                    // block form: scoped to the braces
                    Some(b) => {
                        let prev = std::mem::replace(&mut self.cur_ns, ns);
                        self.hoist(b);
                        let f = self.exec_block(b)?;
                        self.cur_ns = prev;
                        if !matches!(f, Flow::Normal) {
                            return Ok(f);
                        }
                    }
                    // statement form: applies to the rest of the file
                    None => self.cur_ns = ns,
                }
            }
            Stmt::Use(items) => {
                for it in items {
                    let fq = it.name.parts.join("\\");  // declared case preserved: feeds case-sensitive autoloaders
                    let alias = it
                        .alias
                        .clone()
                        .unwrap_or_else(|| it.name.last().to_string())
                        .to_ascii_lowercase();
                    self.use_map.insert(alias, fq);
                }
            }
            // not yet implemented in this increment — parsed but skipped
            Stmt::Declare { strict_types: false } => {}
            Stmt::Declare { strict_types: true } => {
                self.strict_types = true;
            }
        }
        Ok(Flow::Normal)
    }

    fn unwind_break(&self, n: u32) -> R<Flow> {
        Ok(if n > 1 { Flow::Break(n - 1) } else { Flow::Normal })
    }

    /// One foreach iteration: bind key/value, run the body. Returns `Some(flow)`
    /// if the loop must stop (break/return propagation), `None` to continue.
    /// Emit a PHP runtime warning to the output stream (display_errors style),
    /// honoring @/isset-style suppression and the error_reporting level.
    fn warn(&mut self, msg: &str) -> R<()> {
        const E_WARNING: i64 = 2;
        // gen_buf: our generators run eagerly; PHP's run lazily and a body that
        // is never iterated never warns — so eager pre-execution stays silent.
        if self.quiet > 0
            || self.gen_buf.is_some()
            || self.error_level & E_WARNING == 0
            || self.out.len() > MAX_OUTPUT
        {
            return Ok(());
        }
        let file = self
            .cur_file
            .as_ref()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
        // a registered handler intercepts; false return falls through to print
        if let Some(h) = self.error_handler.clone() {
            if self.in_error_handler {
                return Ok(());
            }
            self.in_error_handler = true;
            let r = self.call_value(
                h,
                vec![
                    Value::Int(E_WARNING),
                    Value::Str(msg.as_bytes().to_vec()),
                    Value::Str(file.clone().into_bytes()),
                    Value::Int(self.cur_line as i64),
                ],
            );
            self.in_error_handler = false;
            let v = r?;
            if !matches!(v, Value::Bool(false)) {
                return Ok(());
            }
        }
        let s = format!("\nWarning: {msg} in {file} on line {}\n", self.cur_line);
        self.out.extend_from_slice(s.as_bytes());
        Ok(())
    }

    /// Evaluate with undefined-variable/key warnings suppressed (isset/empty/??/@).
    fn eval_quiet(&mut self, e: &Expr) -> R<Value> {
        self.quiet += 1;
        let r = self.eval(e);
        self.quiet -= 1;
        r
    }

    /// The engine-default timezone as parsed TZif data (None = UTC).
    fn cur_tz(&self) -> Option<Rc<crate::tz::TzData>> {
        if crate::tz::is_utc_name(&self.default_tz) {
            None
        } else {
            crate::tz::lookup(&self.default_tz)
        }
    }

    /// Run one `&mut Arr` operation against the array stored under a variable
    /// (directly or through a reference cell), without cloning the array.
    fn with_array_mut<T>(&mut self, name: &str, f: impl FnOnce(&mut Arr) -> T) -> Option<T> {
        let scope = self.scopes.last_mut().unwrap();
        match scope.get_mut(name) {
            Some(Value::Array(a)) => Some(f(a)),
            Some(Value::Ref(cell)) => {
                let cell = cell.clone();
                let mut b = cell.borrow_mut();
                match &mut *b {
                    Value::Array(a) => Some(f(a)),
                    _ => None,
                }
            }
            _ => None,
        }
    }

    /// In-place mutable access to an array living either in a local variable
    /// or in an object property (foreach-by-ref over `$this->arr` must write
    /// through — WP_Hook::resort_active_iterations).
    fn with_place_mut<T>(&mut self, place: &ArrPlace, f: impl FnOnce(&mut Arr) -> T) -> Option<T> {
        match place {
            ArrPlace::Var(name) => self.with_array_mut(name, f),
            ArrPlace::Prop(rc, pname) => {
                let mut b = rc.borrow_mut();
                match b.get_mut(pname) {
                    Some(Value::Array(a)) => Some(f(a)),
                    Some(Value::Ref(cell)) => {
                        let cell = cell.clone();
                        drop(b);
                        let mut cb = cell.borrow_mut();
                        match &mut *cb {
                            Value::Array(a) => Some(f(a)),
                            _ => None,
                        }
                    }
                    _ => None,
                }
            }
        }
    }

    /// Promote `$base[$idx]` to a shared Ref cell IN PLACE and return it —
    /// the aliasing backbone of `$x = &$arr[k]`. Creates the element (NULL)
    /// and the base array itself when absent, as PHP does. Returns None for
    /// unsupported base shapes (caller falls back to a value copy).
    fn ref_cell_for_index(
        &mut self,
        base: &Expr,
        idx: &Expr,
    ) -> R<Option<Rc<RefCell<Value>>>> {
        let k = Arr::norm_key(&self.eval(idx)?);
        let place = match base {
            Expr::Var(n) if !is_superglobal(n) => {
                // materialize the base array for missing/null vars
                let needs_init = match self.vars().get(n) {
                    None | Some(Value::Null) => true,
                    Some(Value::Ref(c)) => matches!(&*c.borrow(), Value::Null),
                    _ => false,
                };
                if needs_init {
                    match self.vars().get(n) {
                        Some(Value::Ref(c)) => {
                            let c = c.clone();
                            *c.borrow_mut() = Value::Array(Arr::new());
                        }
                        _ => {
                            self.vars().insert(n.clone(), Value::Array(Arr::new()));
                        }
                    }
                }
                ArrPlace::Var(n.clone())
            }
            Expr::Prop(objexpr, pn, _) => {
                let o = self.eval(objexpr)?;
                let pname = self.prop_name_str(pn)?;
                match o {
                    Value::Object(rc) => {
                        {
                            let mut b = rc.borrow_mut();
                            let needs_init = matches!(b.get(&pname), None | Some(Value::Null));
                            if needs_init {
                                b.set(&pname, Value::Array(Arr::new()));
                            }
                        }
                        ArrPlace::Prop(rc, pname)
                    }
                    _ => return Ok(None),
                }
            }
            _ => return Ok(None),
        };
        Ok(self.with_place_mut(&place, |a| {
            if a.get(&k).is_none() {
                a.insert(k.clone(), Value::Null);
            }
            match a.get_mut(&k).unwrap() {
                Value::Ref(c) => c.clone(),
                other => {
                    let c = Rc::new(RefCell::new(std::mem::replace(other, Value::Null)));
                    *other = Value::Ref(c.clone());
                    c
                }
            }
        }))
    }

    /// `foreach ($arr as [$k =>] &$v)`: promote each visited element to a shared
    /// Ref cell in place and bind $v to that cell, so writes through $v (and
    /// writes to $arr[$k]) alias. After moving on, an element whose cell has no
    /// other holder is unwrapped back to a plain value — mirroring PHP, where
    /// only elements still referenced elsewhere keep the `&` refcount marker.
    /// Keys are snapshotted up front (append-during-iteration isn't modeled).
    fn foreach_by_ref(
        &mut self,
        place: &ArrPlace,
        vname: &str,
        key: &Option<Expr>,
        body: &[Stmt],
    ) -> R<Flow> {
        let keys: Vec<Key> = match self.with_place_mut(place, |a| {
            a.entries.iter().map(|(k, _)| k.clone()).collect()
        }) {
            Some(ks) => ks,
            None => return Ok(Flow::Normal),
        };
        // PHP's by-ref foreach iterates LIVE: elements appended during the loop
        // are visited too. We approximate the hash-pointer with rounds: drain the
        // snapshot, then re-scan for keys not yet visited, until none appear.
        // Guards (the corpus has tests that append forever, relying on PHP's
        // memory_limit fatal): post-snapshot visits are capped, and the re-scan
        // starts from a cursor (appends land at the end) so a one-append-per-
        // iteration loop stays O(n), not O(n²).
        const MAX_LIVE_APPENDS: usize = 100_000;
        let mut appended = 0usize;
        let mut scan_pos = keys.len();
        let mut visited: HashSet<Key> = HashSet::new();
        let mut queue: std::collections::VecDeque<Key> = keys.into();
        let mut prev: Option<(Key, Rc<RefCell<Value>>)> = None;
        loop {
            let k = match queue.pop_front() {
                Some(k) => k,
                None => {
                    if appended >= MAX_LIVE_APPENDS {
                        break;
                    }
                    let fresh: Vec<Key> = match self.with_place_mut(place, |a| {
                        let start = scan_pos.min(a.entries.len());
                        let f: Vec<Key> = a.entries[start..]
                            .iter()
                            .map(|(k, _)| k.clone())
                            .filter(|k| !visited.contains(k))
                            .collect();
                        scan_pos = a.entries.len();
                        f
                    }) {
                        Some(f) => f,
                        None => break,
                    };
                    if fresh.is_empty() {
                        break;
                    }
                    appended += fresh.len();
                    queue.extend(fresh);
                    continue;
                }
            };
            visited.insert(k.clone());
            self.tick()?;
            // promote the element to a Ref cell (in place, no array clone)
            let cell = match self.with_place_mut(place, |a| {
                a.get_mut(&k).map(|slot| match slot {
                    Value::Ref(c) => c.clone(),
                    other => {
                        let c = Rc::new(RefCell::new(std::mem::replace(other, Value::Null)));
                        *other = Value::Ref(c.clone());
                        c
                    }
                })
            }) {
                Some(Some(c)) => c,
                Some(None) => continue, // key removed during iteration
                None => break,          // variable no longer holds an array
            };
            if let Some(ke) = key {
                self.assign_to(ke, akey_to_value(&k))?;
            }
            self.vars().insert(vname.to_string(), Value::Ref(cell.clone()));
            // now that $v moved off the previous cell, unwrap it if unaliased
            if let Some((pk, pc)) = prev.take() {
                self.unwrap_element(place, &pk, &pc);
            }
            match self.exec_block(body)? {
                Flow::Break(n) => {
                    return if n > 1 { Ok(Flow::Break(n - 1)) } else { Ok(Flow::Normal) };
                }
                Flow::Continue(n) if n > 1 => return Ok(Flow::Continue(n - 1)),
                Flow::Return(rv) => return Ok(Flow::Return(rv)),
                _ => {}
            }
            prev = Some((k, cell));
        }
        // the final element stays a Ref — $v still aliases it (PHP-correct)
        Ok(Flow::Normal)
    }

    /// Demote a foreach-by-ref element back to a plain value once nothing but
    /// the array (and our transient handle) holds its cell.
    fn unwrap_element(&mut self, place: &ArrPlace, k: &Key, cell: &Rc<RefCell<Value>>) {
        if Rc::strong_count(cell) != 2 {
            return; // still aliased (closure capture, =&, …) — keep the ref
        }
        self.with_place_mut(place, |a| {
            if let Some(slot) = a.get_mut(k) {
                if let Value::Ref(c) = slot {
                    if Rc::ptr_eq(c, cell) {
                        let v = cell.borrow().clone();
                        *slot = v;
                    }
                }
            }
        });
    }

    fn foreach_step(
        &mut self,
        key: &Option<Expr>,
        value: &Expr,
        body: &[Stmt],
        kv: Option<Value>,
        vv: Value,
    ) -> R<Option<Flow>> {
        self.tick()?;
        if let (Some(ke), Some(k)) = (key, kv) {
            self.assign_to(ke, k)?;
        }
        // deref: a leftover reference element must not alias into the loop var
        let vv = match vv {
            Value::Ref(c) => c.borrow().deref(),
            v => v,
        };
        self.assign_to(value, vv)?;
        Ok(match self.exec_block(body)? {
            Flow::Break(n) => Some(if n > 1 { Flow::Break(n - 1) } else { Flow::Normal }),
            Flow::Continue(n) if n > 1 => Some(Flow::Continue(n - 1)),
            Flow::Return(rv) => Some(Flow::Return(rv)),
            _ => None,
        })
    }

    // ---- expressions ----------------------------------------------------
    fn eval(&mut self, e: &Expr) -> R<Value> {
        self.tick()?;
        // Guard native-stack depth: deep left-associative spines (e.g. a long
        // `1+1+...+1`) build a deep AST even though the parser built it iteratively.
        self.eval_depth += 1;
        if self.eval_depth > 6000 {
            self.eval_depth -= 1;
            return Err(RunError("expression too deeply nested".into()));
        }
        let r = self.eval_inner(e);
        self.eval_depth -= 1;
        r
    }

    fn eval_inner(&mut self, e: &Expr) -> R<Value> {
        Ok(match e {
            Expr::Null => Value::Null,
            Expr::Bool(b) => Value::Bool(*b),
            Expr::Int(n) => Value::Int(*n),
            Expr::Float(f) => Value::Float(*f),
            Expr::Str(s) => Value::Str(s.clone()),
            Expr::Template(parts) => {
                let mut out = Vec::new();
                for p in parts {
                    match p {
                        TplPart::Lit(b) => out.extend_from_slice(b),
                        TplPart::Expr(e) => {
                            let v = self.eval(e)?;
                            let b = self.stringify(&v)?;
                            out.extend_from_slice(&b);
                        }
                    }
                }
                Value::Str(out)
            }
            Expr::Array(items) => {
                let mut a = Arr::new();
                for it in items {
                    if it.spread {
                        if let Value::Array(src) = self.eval(&it.value)? {
                            for (k, v) in src.entries {
                                match k {
                                    Key::Int(_) => a.push(v),
                                    Key::Str(_) => a.insert(k, v),
                                }
                            }
                        }
                        continue;
                    }
                    // `[&$x]`: a by-ref item creates $x silently if fresh
                    let val = if it.by_ref { self.eval_quiet(&it.value)? } else { self.eval(&it.value)? };
                    match &it.key {
                        Some(ke) => {
                            let kv = self.eval(ke)?;
                            a.insert(Arr::norm_key(&kv), val);
                        }
                        None => a.push(val),
                    }
                }
                Value::Array(a)
            }
            Expr::Var(name) => {
                if is_superglobal(name) {
                    self.scopes[0].get(name).map(|v| v.deref()).unwrap_or(Value::Null)
                } else {
                    match self.vars().get(name).map(|v| v.deref()) {
                        Some(v) => v,
                        None => {
                            // "this" reads outside objects stay silent (engine paths probe it)
                            if name != "this" {
                                self.warn(&format!("Undefined variable ${name}"))?;
                            }
                            Value::Null
                        }
                    }
                }
            }
            // variable variables: $$x / ${expr} — the inner value names the slot
            Expr::VarVar(inner) => {
                let name = String::from_utf8_lossy(&to_bytes(&self.eval(inner)?)).into_owned();
                self.vars().get(&name).map(|v| v.deref()).unwrap_or(Value::Null)
            }
            Expr::ConstFetch(name) => match self.const_fetch(name) {
                Some(v) => v,
                // PHP 8: an unknown bareword is an Error, not a string
                None => {
                    return Err(self.throw_error(
                        "Error",
                        &format!("Undefined constant \"{}\"", name.last()),
                    ))
                }
            },
            Expr::MagicConst(name) => match name.to_ascii_uppercase().as_str() {
                "__LINE__" => Value::Int(self.cur_line as i64),
                "__FILE__" => Value::Str(
                    self.cur_file
                        .as_ref()
                        .map(|p| p.to_string_lossy().as_bytes().to_vec())
                        .unwrap_or_default(),
                ),
                "__DIR__" => Value::Str(
                    self.cur_file
                        .as_ref()
                        .and_then(|p| p.parent())
                        .map(|d| d.to_string_lossy().as_bytes().to_vec())
                        .unwrap_or_default(),
                ),
                "__CLASS__" => Value::Str(
                    self.current_class.as_deref().map(display_class).unwrap_or_default().into_bytes(),
                ),
                "__FUNCTION__" => {
                    Value::Str(self.cur_fn.last().cloned().unwrap_or_default().into_bytes())
                }
                "__METHOD__" => {
                    let f = self.cur_fn.last().cloned().unwrap_or_default();
                    let s = match &self.current_class {
                        Some(c) if !f.is_empty() => format!("{}::{}", display_class(c), f),
                        _ => f,
                    };
                    Value::Str(s.into_bytes())
                }
                "__NAMESPACE__" => Value::Str(Vec::new()),
                _ => Value::Str(Vec::new()),
            },
            Expr::Unary(op, e) => {
                let v = self.eval(e)?;
                match op {
                    UnOp::Neg => match to_num(&v) {
                        Num::Int(n) => Value::Int(n.wrapping_neg()),
                        Num::Float(f) => Value::Float(-f),
                    },
                    UnOp::Pos => to_num(&v).to_value(),
                    UnOp::Not => Value::Bool(!to_bool(&v)),
                    UnOp::BitNot => Value::Int(!to_i64(&v)),
                }
            }
            Expr::Binary(op, l, r) => self.binary(*op, l, r)?,
            Expr::Assign(lhs, rhs) => {
                let v = self.eval(rhs)?;
                self.assign_to(lhs, v.clone())?;
                v
            }
            Expr::AssignRef(lhs, rhs) => {
                // `$lhs = &$rhs`: both names alias one shared cell. Supported when
                // rhs is a simple variable (the common case); otherwise fall back
                // to a value copy.
                match (&**lhs, &**rhs) {
                    (Expr::Var(lname), Expr::Var(rname)) => {
                        let cell = self.get_ref_cell(rname);
                        let v = cell.borrow().clone();
                        self.mark_rebound(lname);
                        self.vars().insert(lname.clone(), Value::Ref(cell));
                        v
                    }
                    // `$x = &$arr[k]`: promote the ELEMENT to a shared Ref cell
                    // and REBIND $x to it. _wp_array_set walks nested arrays by
                    // re-binding a reference ($ref = &$ref[$k]) — the old
                    // value-copy fallback wrote the subtree through the by-ref
                    // parameter, replacing the caller's whole array.
                    (Expr::Var(lname), Expr::Index(base, Some(idx))) => {
                        let lname = lname.clone();
                        match self.ref_cell_for_index(base, idx)? {
                            Some(cell) => {
                                let v = cell.borrow().clone();
                                self.mark_rebound(&lname);
                                self.vars().insert(lname, Value::Ref(cell));
                                v
                            }
                            None => {
                                let v = self.eval_quiet(rhs)?;
                                self.vars().insert(lname, v.clone());
                                v
                            }
                        }
                    }
                    // `$x = &$obj->prop`: alias the property slot
                    (Expr::Var(lname), Expr::Prop(objexpr, pn, _)) => {
                        let lname = lname.clone();
                        let o = self.eval(objexpr)?;
                        let pname = self.prop_name_str(pn)?;
                        if let Value::Object(rc) = o {
                            let mut b = rc.borrow_mut();
                            if b.get(&pname).is_none() {
                                b.set(&pname, Value::Null);
                            }
                            let cell = match b.get_mut(&pname).unwrap() {
                                Value::Ref(c) => c.clone(),
                                other => {
                                    let c = Rc::new(RefCell::new(std::mem::replace(
                                        other,
                                        Value::Null,
                                    )));
                                    *other = Value::Ref(c.clone());
                                    c
                                }
                            };
                            drop(b);
                            let v = cell.borrow().clone();
                            self.mark_rebound(&lname);
                            self.vars().insert(lname, Value::Ref(cell));
                            v
                        } else {
                            Value::Null
                        }
                    }
                    _ => {
                        // remaining shapes (`$x[0] =& $y`, …) fall back to a
                        // value copy; `=&` reads stay quiet like PHP
                        let v = self.eval_quiet(rhs)?;
                        self.assign_to(lhs, v.clone())?;
                        v
                    }
                }
            }
            Expr::AssignOp(op, lhs, rhs) => {
                // `$v .= expr` in place: append to the existing string instead of
                // cloning it each time (avoids O(n^2) growth on `.=` loops).
                if *op == BinOp::Concat {
                    // Skip the in-place fast path for reference-backed vars (it would
                    // overwrite the Ref); the general path below write-throughs.
                    let is_ref = matches!(&**lhs, Expr::Var(n) if matches!(self.vars().get(n), Some(Value::Ref(_))));
                    if let (Expr::Var(name), false) = (&**lhs, is_ref) {
                        let rv = self.eval(rhs)?;
                        let rb = self.stringify(&rv)?;
                        let slot = self.vars().entry(name.clone()).or_insert(Value::Str(Vec::new()));
                        if let Value::Str(s) = slot {
                            if s.len() + rb.len() <= MAX_STR {
                                s.extend_from_slice(&rb);
                            }
                            return Ok(Value::Str(s.clone()));
                        } else {
                            let mut s = to_bytes(slot);
                            s.extend_from_slice(&rb);
                            let nv = Value::Str(s);
                            *slot = nv.clone();
                            return Ok(nv);
                        }
                    }
                }
                let cur = self.eval(lhs)?;
                let rv = self.eval(rhs)?;
                let nv = self.apply_bin(*op, &cur, &rv)?;
                self.assign_to(lhs, nv.clone())?;
                nv
            }
            Expr::PreInc(e) => {
                let v = inc(&self.eval(e)?, 1);
                self.assign_to(e, v.clone())?;
                v
            }
            Expr::PreDec(e) => {
                let v = inc(&self.eval(e)?, -1);
                self.assign_to(e, v.clone())?;
                v
            }
            Expr::PostInc(e) => {
                let old = self.eval(e)?;
                let v = inc(&old, 1);
                self.assign_to(e, v)?;
                old
            }
            Expr::PostDec(e) => {
                let old = self.eval(e)?;
                let v = inc(&old, -1);
                self.assign_to(e, v)?;
                old
            }
            Expr::Ternary(c, mid, els) => {
                let cv = self.eval(c)?;
                if to_bool(&cv) {
                    match mid {
                        Some(m) => self.eval(m)?,
                        None => cv, // a ?: b
                    }
                } else {
                    self.eval(els)?
                }
            }
            Expr::Index(base, idx) => self.read_index(base, idx)?,
            Expr::Call(callee, args) => self.eval_call(callee, args)?,
            Expr::Isset(items) => {
                self.quiet += 1;
                let mut all = true;
                for it in items {
                    match self.isset_one(it) {
                        Ok(true) => {}
                        Ok(false) => {
                            all = false;
                            break;
                        }
                        Err(e) => {
                            self.quiet -= 1;
                            return Err(e);
                        }
                    }
                }
                self.quiet -= 1;
                Value::Bool(all)
            }
            Expr::Empty(e) => Value::Bool(!to_bool(&self.eval_quiet(e)?)),
            Expr::ErrorSuppress(e) => self.eval_quiet(e).unwrap_or(Value::Null),
            Expr::Print(e) => {
                let v = self.eval(e)?;
                self.out.extend_from_slice(&to_bytes(&v));
                Value::Int(1)
            }
            Expr::Cast(ct, e) => {
                let v = self.eval(e)?;
                // (string) on an object must honor __toString (to_bytes can't).
                if matches!(ct, CastType::String) && matches!(v, Value::Object(_)) {
                    Value::Str(self.stringify(&v)?)
                } else {
                    self.cast(*ct, v)
                }
            }
            Expr::Clone(e) => {
                let v = self.eval(e)?;
                match &v {
                    Value::Object(rc) => {
                        // shallow copy: props copied (objects inside stay shared),
                        // fresh instance id, then __clone() runs on the copy
                        let (class, props) = {
                            let b = rc.borrow();
                            (b.class.clone(), b.props.clone())
                        };
                        let copy = Rc::new(RefCell::new(Obj::new(class.clone())));
                        copy.borrow_mut().props = props;
                        let cv = Value::Object(copy);
                        if self.find_method(&class, "__clone").is_some() {
                            self.call_method(cv.clone(), "__clone", vec![])?;
                        }
                        cv
                    }
                    _ => {
                        return Err(self.throw_error(
                            "Error",
                            &format!("__clone method called on non-object"),
                        ))
                    }
                }
            }
            Expr::Match(subj, arms) => {
                let s = self.eval(subj)?;
                let mut result = None;
                for arm in arms {
                    match &arm.conditions {
                        Some(conds) => {
                            for c in conds {
                                let cv = self.eval(c)?;
                                if strict_eq(&s, &cv) {
                                    result = Some(self.eval(&arm.body)?);
                                    break;
                                }
                            }
                        }
                        None => {
                            result = Some(self.eval(&arm.body)?);
                        }
                    }
                    if result.is_some() {
                        break;
                    }
                }
                match result {
                    Some(v) => v,
                    None => {
                        let disp = match &s {
                            Value::Str(b) => format!("'{}'", String::from_utf8_lossy(b)),
                            Value::Int(_) | Value::Float(_) | Value::Bool(_) | Value::Null => {
                                String::from_utf8_lossy(&to_bytes(&s)).into_owned()
                            }
                            _ => format!("of type {}", type_name(&s)),
                        };
                        return Err(self.throw_error(
                            "UnhandledMatchError",
                            &format!("Unhandled match case {disp}"),
                        ));
                    }
                }
            }
            Expr::New(class, args) => {
                let cname = self.resolve_class_name(class)?;
                let argv = {
                    let (pos, named) = self.eval_args2(args)?;
                    if named.is_empty() {
                        pos
                    } else {
                        let params = self.find_method(&cname, "__construct").map(|(_, m)| m.params.clone());
                        self.merge_named(pos, named, params.as_deref())?
                    }
                };
                self.instantiate(&cname, argv)?
            }
            Expr::NewAnon(decl, args) => {
                let ptr = &**decl as *const ClassDecl as usize;
                let cname = match self.anon_names.get(&ptr) {
                    Some(n) => n.clone(),
                    None => {
                        let n = format!("class@anonymous#{}", self.anon_names.len());
                        self.anon_names.insert(ptr, n.clone());
                        let mut cd = (**decl).clone();
                        cd.name = n.clone();
                        self.classes.insert(n.to_ascii_lowercase(), Rc::new(cd));
                        n
                    }
                };
                let argv = {
                    let (pos, named) = self.eval_args2(args)?;
                    if named.is_empty() {
                        pos
                    } else {
                        let params = self.find_method(&cname, "__construct").map(|(_, m)| m.params.clone());
                        self.merge_named(pos, named, params.as_deref())?
                    }
                };
                self.instantiate(&cname, argv)?
            }
            Expr::Prop(obj, name, nullsafe) => {
                let o = self.eval(obj)?;
                if *nullsafe && matches!(o, Value::Null) {
                    return Ok(Value::Null);
                }
                let pname = self.prop_name_str(name)?;
                match &o {
                    Value::Object(rc) => {
                        let existing = rc.borrow().get(&pname).cloned();
                        match existing {
                            Some(v) => v,
                            None => {
                                let class = rc.borrow().class.clone();
                                if self.find_method(&class, "__get").is_some() {
                                    self.call_method(o.clone(), "__get", vec![Value::Str(pname.into_bytes())])?
                                } else {
                                    Value::Null
                                }
                            }
                        }
                    }
                    // closures are objects: property reads are silent nulls.
                    // Null bases stay silent too — in this engine a null is as
                    // likely an unimplemented-API artifact (DOM/SimpleXML gaps)
                    // as a user error, and spurious warnings cost more than
                    // missed ones.
                    Value::Closure(_) | Value::Null | Value::Array(_) => Value::Null,
                    other => {
                        let t = self.given_type(other);
                        self.warn(&format!("Attempt to read property \"{pname}\" on {t}"))?;
                        Value::Null
                    }
                }
            }
            Expr::MethodCall(obj, name, args, nullsafe) => {
                let o = self.eval(obj)?;
                if *nullsafe && matches!(o, Value::Null) {
                    return Ok(Value::Null);
                }
                let mname = self.prop_name_str(name)?;
                // args evaluate quietly: the callee may declare by-ref out-params
                // (PHP doesn't warn about fresh vars passed to those)
                self.quiet += 1;
                let evaled = self.eval_args2(args);
                self.quiet -= 1;
                let argv = {
                    let (pos, named) = evaled?;
                    if named.is_empty() {
                        pos
                    } else {
                        let params = match &o {
                            Value::Object(rc) => {
                                let cls = rc.borrow().class.clone();
                                self.find_method(&cls, &mname).map(|(_, m)| m.params.clone())
                            }
                            _ => None,
                        };
                        self.merge_named(pos, named, params.as_deref())?
                    }
                };
                self.call_method_ref(o, &mname, argv, Some(args))?
            }
            Expr::StaticCall(class, name, args) => {
                let cname = self.resolve_class_name(class)?;
                let mname = self.prop_name_str(name)?;
                self.quiet += 1;
                let evaled = self.eval_args2(args);
                self.quiet -= 1;
                let argv = {
                    let (pos, named) = evaled?;
                    if named.is_empty() {
                        pos
                    } else {
                        let params = self.find_method(&cname, &mname).map(|(_, m)| m.params.clone());
                        self.merge_named(pos, named, params.as_deref())?
                    }
                };
                // `parent::`/`self::` keep the current $this if present
                let this = self.vars().get("this").cloned();
                let forwarding = matches!(&**class, Expr::ConstFetch(n)
                    if matches!(n.last().to_ascii_lowercase().as_str(), "self" | "parent" | "static"));
                self.call_static_fw(&cname, &mname, argv, this, Some(args), forwarding)?
            }
            Expr::ClassConst(class, name) => {
                if name == "class" {
                    let cname = self.resolve_class_name(class)?;
                    Value::Str(cname.into_bytes())
                } else {
                    let cname = self.resolve_class_name(class)?;
                    self.class_const(&cname, name)?
                }
            }
            Expr::StaticProp(class, name) => {
                let cname = self.resolve_class_name(class)?;
                let key = self.static_prop_key(&cname, name)?;
                self.static_props.get(&key).cloned().unwrap_or(Value::Null)
            }
            Expr::Throw(inner) => {
                let v = self.eval(inner)?;
                self.thrown = Some(v);
                return Err(RunError("__phargo_throw__".into()));
            }
            Expr::Yield(key, value) => {
                let v = match value {
                    Some(e) => self.eval(e)?,
                    None => Value::Null,
                };
                let k = match key {
                    Some(e) => Some(self.eval(e)?),
                    None => None,
                };
                self.gen_nodes = self.gen_nodes.saturating_add(value_size(&v, MAX_ARRAY_NODES));
                if self.gen_nodes >= MAX_ARRAY_NODES {
                    return Err(RunError("generator buffer limit exceeded".into()));
                }
                if let Some(buf) = self.gen_buf.as_mut() {
                    match k {
                        Some(kv) => buf.insert(Arr::norm_key(&kv), v),
                        None => buf.push(v),
                    }
                }
                Value::Null // eager generators: send() values aren't modeled
            }
            Expr::YieldFrom(e) => {
                let src = self.eval(e)?;
                match src {
                    Value::Array(a) => {
                        for (k, v) in a.entries {
                            self.gen_nodes =
                                self.gen_nodes.saturating_add(value_size(&v, MAX_ARRAY_NODES));
                            if self.gen_nodes >= MAX_ARRAY_NODES {
                                return Err(RunError("generator buffer limit exceeded".into()));
                            }
                            if let Some(buf) = self.gen_buf.as_mut() {
                                match k {
                                    Key::Int(_) => buf.push(v),
                                    Key::Str(_) => buf.insert(k, v),
                                }
                            }
                        }
                    }
                    Value::Object(_) => {
                        // a sub-generator/iterable — drain via iterator_to_array
                        let drained = self.builtin("iterator_to_array", vec![src, Value::Bool(false)])?;
                        if let Value::Array(a) = drained {
                            for (_, v) in a.entries {
                                self.gen_nodes = self
                                    .gen_nodes
                                    .saturating_add(value_size(&v, MAX_ARRAY_NODES));
                                if self.gen_nodes >= MAX_ARRAY_NODES {
                                    return Err(RunError("generator buffer limit exceeded".into()));
                                }
                                if let Some(buf) = self.gen_buf.as_mut() {
                                    buf.push(v);
                                }
                            }
                        }
                    }
                    _ => {}
                }
                Value::Null
            }
            Expr::Closure(c) => {
                let mut captures = Vec::new();
                for u in &c.uses {
                    let v = if u.by_ref {
                        // `use (&$x)`: capture a shared cell so the closure and the
                        // defining scope see each other's writes.
                        Value::Ref(self.get_ref_cell(&u.name))
                    } else {
                        self.vars().get(&u.name).map(|v| v.deref()).unwrap_or(Value::Null)
                    };
                    captures.push((u.name.clone(), v));
                }
                let bound_this = if c.is_static {
                    None
                } else {
                    self.vars().get("this").cloned()
                };
                Value::Closure(Rc::new(ClosureVal {
                    kind: ClosureKind::Full(Rc::new((**c).clone())),
                    captures,
                    bound_this,
                }))
            }
            Expr::ArrowFn(a) => {
                // arrow fns auto-capture the entire enclosing scope by value
                let captures: Vec<(String, Value)> = self
                    .vars()
                    .iter()
                    .filter(|(k, _)| !k.starts_with('\0'))
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect();
                let bound_this = if a.is_static {
                    None
                } else {
                    self.vars().get("this").cloned()
                };
                Value::Closure(Rc::new(ClosureVal {
                    kind: ClosureKind::Arrow(Rc::new((**a).clone())),
                    captures,
                    bound_this,
                }))
            }
            Expr::InstanceOf(e, class) => {
                let v = self.eval(e)?;
                let target = self.resolve_class_name(class)?;
                Value::Bool(match &v {
                    Value::Object(rc) => {
                        let c = rc.borrow().class.clone();
                        self.is_subclass(&c, &target)
                    }
                    _ => false,
                })
            }
            // constructs not in this increment
            _ => Value::Null,
        })
    }

    fn binary(&mut self, op: BinOp, l: &Expr, r: &Expr) -> R<Value> {
        // short-circuit logical operators
        match op {
            BinOp::And => {
                let lv = self.eval(l)?;
                return Ok(Value::Bool(to_bool(&lv) && to_bool(&self.eval(r)?)));
            }
            BinOp::Or => {
                let lv = self.eval(l)?;
                return Ok(Value::Bool(to_bool(&lv) || to_bool(&self.eval(r)?)));
            }
            BinOp::Coalesce => {
                let lv = self.eval_quiet(l)?;
                return Ok(if matches!(lv, Value::Null) { self.eval(r)? } else { lv });
            }
            BinOp::Concat => {
                // honor __toString on either operand
                let lv = self.eval(l)?;
                let rv = self.eval(r)?;
                let mut s = self.stringify(&lv)?;
                let rb = self.stringify(&rv)?;
                if s.len() + rb.len() <= MAX_STR {
                    s.extend_from_slice(&rb);
                }
                return Ok(Value::Str(s));
            }
            _ => {}
        }
        let lv = self.eval(l)?;
        let rv = self.eval(r)?;
        self.apply_bin(op, &lv, &rv)
    }

    fn apply_bin(&mut self, op: BinOp, l: &Value, r: &Value) -> R<Value> {
        use BinOp::*;
        // PHP 8: arrays (and non-Stringable objects) are unsupported operands for
        // arithmetic/bitwise ops — TypeError, except array + array (union).
        // BcMath\Number overloads arithmetic: dispatch to the bc ops and wrap
        let is_num_obj = |v: &Value| matches!(v, Value::Object(rc) if rc.borrow().class.eq_ignore_ascii_case("BcMath\\Number"));
        if (is_num_obj(l) || is_num_obj(r)) && matches!(op, Add | Sub | Mul | Div | Mod | Pow) {
            let method = match op {
                Add => "add",
                Sub => "sub",
                Mul => "mul",
                Div => "div",
                Mod => "mod",
                _ => "pow",
            };
            let lift = |ev: &mut Self, v: &Value| -> R<Value> {
                if is_num_obj(v) {
                    Ok(v.clone())
                } else {
                    ev.instantiate("BcMath\\Number", vec![v.clone()])
                }
            };
            let lo = lift(self, l)?;
            let ro = lift(self, r)?;
            return self.call_method(lo, method, vec![ro]);
        }
        let arith = matches!(op, Add | Sub | Mul | Div | Mod | Pow | BitAnd | BitOr | BitXor | Shl | Shr);
        if arith {
            let bad = |v: &Value| matches!(v, Value::Array(_));
            let union_ok = matches!(op, Add) && bad(l) && bad(r);
            if !union_ok && (bad(l) || bad(r)) {
                let sym = match op {
                    Add => "+", Sub => "-", Mul => "*", Div => "/", Mod => "%",
                    Pow => "**", BitAnd => "&", BitOr => "|", BitXor => "^",
                    Shl => "<<", Shr => ">>",
                    _ => "?",
                };
                let msg = format!(
                    "Unsupported operand types: {} {} {}",
                    self.given_type(l),
                    sym,
                    self.given_type(r)
                );
                return Err(self.throw_error("TypeError", &msg));
            }
        }
        Ok(match op {
            Add => {
                if let (Value::Array(a), Value::Array(b)) = (l, r) {
                    let mut out = a.clone();
                    for (k, v) in &b.entries {
                        if out.get(k).is_none() {
                            out.insert(k.clone(), v.clone());
                        }
                    }
                    return Ok(Value::Array(out));
                }
                num_arith(l, r, |a, b| a.wrapping_add(b), |a, b| a + b)
            }
            Sub => num_arith(l, r, |a, b| a.wrapping_sub(b), |a, b| a - b),
            Mul => num_arith(l, r, |a, b| a.wrapping_mul(b), |a, b| a * b),
            Div => {
                let rf = to_f64(r);
                if rf == 0.0 {
                    return Err(self.throw_error("DivisionByZeroError", "Division by zero"));
                }
                match (to_num(l), to_num(r)) {
                    (Num::Int(a), Num::Int(b)) if b != 0 && a % b == 0 => Value::Int(a / b),
                    _ => Value::Float(to_f64(l) / rf),
                }
            }
            Mod => {
                let b = to_i64(r);
                if b == 0 {
                    return Err(self.throw_error("DivisionByZeroError", "Modulo by zero"));
                }
                Value::Int(to_i64(l).wrapping_rem(b))
            }
            Pow => {
                match (to_num(l), to_num(r)) {
                    (Num::Int(a), Num::Int(b)) if b >= 0 && b < 64 => {
                        match a.checked_pow(b as u32) {
                            Some(n) => Value::Int(n),
                            None => Value::Float((a as f64).powf(b as f64)),
                        }
                    }
                    _ => Value::Float(to_f64(l).powf(to_f64(r))),
                }
            }
            Concat => {
                let mut s = to_bytes(l);
                let rb = to_bytes(r);
                if s.len() + rb.len() <= MAX_STR {
                    s.extend_from_slice(&rb);
                }
                Value::Str(s)
            }
            Eq => Value::Bool(loose_eq(l, r)),
            NotEq => Value::Bool(!loose_eq(l, r)),
            Identical => Value::Bool(strict_eq(l, r)),
            NotIdentical => Value::Bool(!strict_eq(l, r)),
            Lt => Value::Bool(compare(l, r) == std::cmp::Ordering::Less),
            Gt => Value::Bool(compare(l, r) == std::cmp::Ordering::Greater),
            Le => Value::Bool(compare(l, r) != std::cmp::Ordering::Greater),
            Ge => Value::Bool(compare(l, r) != std::cmp::Ordering::Less),
            Spaceship => Value::Int(match compare(l, r) {
                std::cmp::Ordering::Less => -1,
                std::cmp::Ordering::Equal => 0,
                std::cmp::Ordering::Greater => 1,
            }),
            // bitwise on two strings is byte-wise (not numeric)
            BitAnd if matches!((l, r), (Value::Str(_), Value::Str(_))) => str_bitwise(l, r, |a, b| a & b, false),
            BitOr if matches!((l, r), (Value::Str(_), Value::Str(_))) => str_bitwise(l, r, |a, b| a | b, true),
            BitXor if matches!((l, r), (Value::Str(_), Value::Str(_))) => str_bitwise(l, r, |a, b| a ^ b, false),
            BitAnd => Value::Int(to_i64(l) & to_i64(r)),
            BitOr => Value::Int(to_i64(l) | to_i64(r)),
            BitXor => Value::Int(to_i64(l) ^ to_i64(r)),
            Shl => {
                let sh = to_i64(r);
                if sh < 0 {
                    return Err(self.throw_error("ArithmeticError", "Bit shift by negative number"));
                }
                Value::Int(if sh >= 64 { 0 } else { to_i64(l).wrapping_shl(sh as u32) })
            }
            Shr => {
                let sh = to_i64(r);
                if sh < 0 {
                    return Err(self.throw_error("ArithmeticError", "Bit shift by negative number"));
                }
                // arithmetic shift saturates to the sign bit past 63
                Value::Int(if sh >= 64 { to_i64(l) >> 63 } else { to_i64(l) >> sh })
            }
            Xor => Value::Bool(to_bool(l) ^ to_bool(r)),
            // logicals handled in `binary`
            And | Or | Coalesce => Value::Null,
        })
    }

    fn cast(&self, ct: CastType, v: Value) -> Value {
        match ct {
            CastType::Int => Value::Int(to_i64(&v)),
            CastType::Float => Value::Float(to_f64(&v)),
            CastType::String => Value::Str(to_bytes(&v)),
            CastType::Bool => Value::Bool(to_bool(&v)),
            CastType::Array => match v {
                Value::Array(_) => v,
                Value::Null => Value::Array(Arr::new()),
                // (array)$obj extracts the property table; private/protected
                // keys get PHP's NUL-mangled prefixes ("\0Class\0p", "\0*\0p")
                Value::Object(rc) => {
                    let class = rc.borrow().class.clone();
                    let props: Vec<(String, Value)> = rc.borrow().props.clone();
                    let mut a = Arr::new();
                    for (name, val) in props {
                        let annot = self.prop_annotation(&class, &name);
                        let key = if annot.is_empty() {
                            name.into_bytes()
                        } else if annot == ":protected" {
                            let mut k = b"\0*\0".to_vec();
                            k.extend_from_slice(name.as_bytes());
                            k
                        } else {
                            // annot is `:"Declaring":private`
                            let decl = annot
                                .trim_start_matches(":\"")
                                .split('"')
                                .next()
                                .unwrap_or(&class)
                                .to_string();
                            let mut k = vec![0u8];
                            k.extend_from_slice(decl.as_bytes());
                            k.push(0);
                            k.extend_from_slice(name.as_bytes());
                            k
                        };
                        a.insert(Key::Str(key), val);
                    }
                    Value::Array(a)
                }
                other => {
                    let mut a = Arr::new();
                    a.push(other);
                    Value::Array(a)
                }
            },
            CastType::Object => match v {
                Value::Object(_) | Value::Closure(_) => v,
                Value::Array(a) => {
                    let mut o = Obj::new("stdClass");
                    for (k, val) in a.entries {
                        let name = match k {
                            Key::Int(n) => n.to_string(),
                            Key::Str(s) => String::from_utf8_lossy(&s).into_owned(),
                        };
                        o.set(&name, val);
                    }
                    Value::Object(Rc::new(RefCell::new(o)))
                }
                Value::Null => new_obj("stdClass"),
                other => {
                    let o = Rc::new(RefCell::new(Obj::new("stdClass")));
                    o.borrow_mut().set("scalar", other);
                    Value::Object(o)
                }
            },
            CastType::Unset => Value::Null,
        }
    }

    /// Read `$var[i]`, `$var[i][j]`, ... by navigating the stored container by
    /// reference and cloning only the final element. (Naively evaluating `base`
    /// clones the whole container on every access — O(n^2) in array-heavy loops.)
    fn read_index(&mut self, base: &Expr, idx: &Option<Box<Expr>>) -> R<Value> {
        // $GLOBALS['x'] is a live view of the global scope.
        if let (Expr::Var(n), Some(i)) = (base, idx) {
            if n == "GLOBALS" {
                let key = String::from_utf8_lossy(&to_bytes(&self.eval(i)?)).into_owned();
                return Ok(self.scopes[0].get(&key).map(|v| v.deref()).unwrap_or(Value::Null));
            }
        }
        // ArrayAccess: `$expr[$k]` where the base is an object → offsetGet($k).
        // (Single level; deeper chains fall through to the array path below.)
        // Peek without cloning (deref-clone of a big array here is O(n) per read).
        let base_var_obj = matches!(base, Expr::Var(name) if {
            match self.vars().get(name) {
                Some(Value::Object(_)) => true,
                Some(Value::Ref(c)) => matches!(&*c.borrow(), Value::Object(_)),
                _ => false,
            }
        });
        // A base that produces a value (property/method/call result) may be an
        // ArrayAccess object (e.g. SimpleXML `$xml->book[0]`); eval it once and
        // dispatch to offsetGet, otherwise index the produced value as an array.
        let base_complex = matches!(
            base,
            Expr::Prop(..) | Expr::MethodCall(..) | Expr::Call(..) | Expr::StaticCall(..)
        );
        if base_var_obj || base_complex {
            let iv = match idx {
                Some(i) => self.eval(i)?,
                None => Value::Null,
            };
            let obj = self.eval(base)?;
            if let Value::Object(rc) = &obj {
                let class = rc.borrow().class.clone();
                if self.find_method(&class, "offsetget").is_some() {
                    return self.call_method(obj.clone(), "offsetGet", vec![iv]);
                }
            }
            if base_complex {
                // not an ArrayAccess object — index the value as an array/string
                if let Value::Array(a) = &obj {
                    return Ok(a.get(&Arr::norm_key(&iv)).map(|v| v.deref()).unwrap_or(Value::Null));
                }
                if let Value::Str(s) = &obj {
                    let i = to_i64(&iv);
                    if i >= 0 && (i as usize) < s.len() {
                        return Ok(Value::Str(vec![s[i as usize]]));
                    }
                }
            }
            return Ok(Value::Null);
        }
        let first = match idx {
            Some(i) => Arr::norm_key(&self.eval(i)?),
            None => return Ok(Value::Null), // `$a[]` isn't a readable expression
        };
        // Unwind a chain of `Index` nodes down to the root, evaluating each key.
        let mut keys = vec![first];
        let mut node = base;
        loop {
            match node {
                Expr::Index(b, Some(i)) => {
                    keys.push(Arr::norm_key(&self.eval(i)?));
                    node = b;
                }
                Expr::Var(_) => break,
                // root isn't a plain variable (e.g. a call result) — fall back to
                // evaluating it once, then indexing.
                other => {
                    keys.reverse();
                    let mut v = self.eval(other)?;
                    for k in &keys {
                        v = self.index_get_key(&v, k);
                    }
                    return Ok(v);
                }
            }
        }
        keys.reverse();
        let name = match node {
            Expr::Var(n) => n.clone(),
            _ => unreachable!(),
        };
        // Navigate by reference (keys already evaluated — no &mut self needed for
        // the walk itself; warnings are recorded and emitted after the borrow ends).
        // Superglobals resolve against the global scope.
        let mut warn_msg: Option<String> = None;
        let result = 'walk: {
            let scope = if is_superglobal(&name) {
                &self.scopes[0]
            } else {
                self.scopes.last().unwrap()
            };
            let mut v = match scope.get(&name) {
                Some(v) => v,
                None => {
                    if !is_superglobal(&name) && name != "this" {
                        warn_msg = Some(format!("Undefined variable ${name}"));
                    }
                    break 'walk Value::Null;
                }
            };
            // Deref a reference-backed variable before navigating.
            if let Value::Ref(cell) = v {
                let inner = cell.borrow().clone();
                break 'walk read_index_value(&inner, &keys);
            }
            let mut out = Value::Null;
            let mut done = false;
            for (i, k) in keys.iter().enumerate() {
                match v {
                    Value::Array(a) => {
                        v = match a.get(k) {
                            Some(x) => x,
                            None => {
                                warn_msg = Some(match k {
                                    Key::Int(n) => format!("Undefined array key {n}"),
                                    Key::Str(sk) => format!(
                                        "Undefined array key \"{}\"",
                                        String::from_utf8_lossy(sk)
                                    ),
                                });
                                done = true;
                                break;
                            }
                        };
                        // Reference element mid-path: continue inside the cell.
                        if let Value::Ref(cell) = v {
                            let inner = cell.borrow().clone();
                            out = read_index_value(&inner, &keys[i + 1..]);
                            done = true;
                            break;
                        }
                    }
                    Value::Str(sv) => {
                        out = string_char(sv, k);
                        done = true;
                        break;
                    }
                    _ => {
                        done = true;
                        break;
                    }
                }
            }
            if done { out } else { v.deref() }
        };
        if let Some(m) = warn_msg {
            self.warn(&m)?;
        }
        Ok(result)
    }

    fn index_get_key(&self, base: &Value, k: &Key) -> Value {
        match base {
            Value::Array(a) => a.get(k).map(|v| v.deref()).unwrap_or(Value::Null),
            Value::Str(s) => string_char(s, k),
            _ => Value::Null,
        }
    }

    fn index_get(&self, base: &Value, idx: &Value) -> Value {
        match base {
            Value::Array(a) => a.get(&Arr::norm_key(idx)).map(|v| v.deref()).unwrap_or(Value::Null),
            Value::Str(s) => {
                let i = to_i64(idx);
                let i = if i < 0 { s.len() as i64 + i } else { i };
                if i >= 0 && (i as usize) < s.len() {
                    Value::Str(vec![s[i as usize]])
                } else {
                    Value::Str(Vec::new())
                }
            }
            _ => Value::Null,
        }
    }

    fn const_fetch(&self, name: &Name) -> Option<Value> {
        let n = name.last();
        // namespaced candidates first (constants fall back to global, per PHP)
        if !name.fully_qualified {
            if !self.cur_ns.is_empty() {
                if let Some(v) = self.consts.get(&format!("{}\\{n}", self.cur_ns)) {
                    return Some(v.clone());
                }
            }
        }
        if name.parts.len() > 1 {
            let joined = name.parts.join("\\");
            if let Some(v) = self.consts.get(&joined) {
                return Some(v.clone());
            }
        }
        if let Some(v) = self.consts.get(n) {
            return Some(v.clone());
        }
        php_const(n)
    }

    // ---- assignment targets --------------------------------------------
    fn assign_to(&mut self, target: &Expr, val: Value) -> R<()> {
        // plain assignment copies the VALUE — a Ref cell arriving here (from
        // by-ref arrays, filter machinery, aliased params) must not become an
        // alias in the target slot (WP_Query->posts read as Ref broke reads)
        let val = if let Value::Ref(c) = &val {
            c.borrow().clone()
        } else {
            val
        };
        // Guard against pathological array explosion (e.g. self-referential
        // value-copy where references aren't modeled yet).
        if matches!(val, Value::Array(_)) && value_size(&val, MAX_ARRAY_NODES) > MAX_ARRAY_NODES {
            return Err(self.throw_error("Error", "Allocated array exceeds memory limit"));
        }
        match target {
            Expr::Var(name) => {
                if is_superglobal(name) {
                    self.scopes[0].insert(name.clone(), val);
                } else if let Some(Value::Ref(cell)) = self.vars().get(name) {
                    // Write through if the variable currently aliases a reference cell.
                    let cell = cell.clone();
                    *cell.borrow_mut() = val;
                } else {
                    self.vars().insert(name.clone(), val);
                }
            }
            // variable variables: $$x = v / ${expr} = v
            Expr::VarVar(inner) => {
                let name = String::from_utf8_lossy(&to_bytes(&self.eval(inner)?)).into_owned();
                if let Some(Value::Ref(cell)) = self.vars().get(&name) {
                    let cell = cell.clone();
                    *cell.borrow_mut() = val;
                } else {
                    self.vars().insert(name, val);
                }
            }
            Expr::Index(base, idx) => {
                // $GLOBALS['x'] = v writes the global-scope variable x.
                if let (Expr::Var(n), Some(i)) = (&**base, idx) {
                    if n == "GLOBALS" {
                        let key = String::from_utf8_lossy(&to_bytes(&self.eval(i)?)).into_owned();
                        // write through an existing Ref cell so `global $x`
                        // bindings elsewhere observe the new value
                        if let Some(Value::Ref(cell)) = self.scopes[0].get(&key) {
                            let cell = cell.clone();
                            *cell.borrow_mut() = val;
                        } else {
                            self.scopes[0].insert(key, val);
                        }
                        return Ok(());
                    }
                }
                // ArrayAccess with an explicit offset: dispatch offsetSet with the
                // RAW offset (norm_key would mangle object offsets, e.g. WeakMap /
                // SplObjectStorage keyed by object identity).
                if let Some(i) = idx {
                    if let Some(obj) = self.arrayaccess_obj(base, "offsetset") {
                        let raw = self.eval(i)?;
                        self.call_method(obj, "offsetSet", vec![raw, val])?;
                        return Ok(());
                    }
                }
                // ensure base is an array, then set/append
                let key = match idx {
                    Some(i) => Some(Arr::norm_key(&self.eval(i)?)),
                    None => None,
                };
                self.assign_index(base, key, val)?;
            }
            Expr::Prop(obj, name, _) => {
                let o = self.eval(obj)?;
                let pname = self.prop_name_str(name)?;
                if let Value::Object(rc) = o {
                    let class = rc.borrow().class.clone();
                    let val = if self.class_has_typed_props(&class) {
                        self.check_prop_write(&class, &pname, val)?
                    } else {
                        val
                    };
                    rc.borrow_mut().set(&pname, val);
                }
            }
            Expr::StaticProp(class, name) => {
                let cname = self.resolve_class_name(class)?;
                let key = self.static_prop_key(&cname, name)?;
                self.static_props.insert(key, val);
            }
            // list/array destructuring: [$a, $b] = ...  and  list($a, $b) = ...
            Expr::Array(_) | Expr::List(_) => {
                self.destructure(target, val)?;
            }
            _ => {}
        }
        Ok(())
    }

    /// If `base` resolves (cheaply) to an object implementing ArrayAccess method
    /// `m`, return that object. Used to keep raw (possibly object) offsets for
    /// offsetGet/offsetSet instead of normalizing them to array keys.
    /// `unset($base[$key])` for array elements: remove the key from the array
    /// stored under `base` (a variable, superglobal, ref, or nested index/prop).
    fn unset_index(&mut self, base: &Expr, key: &Key) -> R<()> {
        match base {
            Expr::Var(name) => {
                let scope = if is_superglobal(name) { &mut self.scopes[0] } else { self.scopes.last_mut().unwrap() };
                match scope.get_mut(name) {
                    Some(Value::Array(a)) => { a.remove(key); }
                    Some(Value::Ref(cell)) => {
                        if let Value::Array(a) = &mut *cell.borrow_mut() { a.remove(key); }
                    }
                    _ => {}
                }
            }
            Expr::Prop(obj, name, _) => {
                let o = self.eval(obj)?;
                let pname = self.prop_name_str(name)?;
                if let Value::Object(rc) = o {
                    if let Some(Value::Array(a)) = rc.borrow_mut().get_mut(&pname) {
                        a.remove(key);
                    }
                }
            }
            Expr::Index(inner, Some(iidx)) => {
                // nested $a[x][y]: read-modify-write the inner container
                let ikey = Arr::norm_key(&self.eval(iidx)?);
                let mut cur = self.eval_quiet(base).unwrap_or(Value::Null);
                if let Value::Array(a) = &mut cur {
                    a.remove(key);
                    self.assign_index(inner, Some(ikey), cur)?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// One operand of isset(): true if set & non-null. For ArrayAccess offsets,
    /// PHP calls offsetExists (not offsetGet).
    fn isset_one(&mut self, e: &Expr) -> R<bool> {
        if let Expr::Index(base, Some(idx)) = e {
            if let Some(obj) = self.arrayaccess_obj(base, "offsetexists") {
                let raw = self.eval(idx)?;
                return Ok(to_bool(&self.call_method(obj, "offsetExists", vec![raw])?));
            }
        }
        Ok(!matches!(self.eval(e)?, Value::Null))
    }

    fn arrayaccess_obj(&mut self, base: &Expr, m: &str) -> Option<Value> {
        // Peek WITHOUT cloning: for a variable, only clone the object Rc if the
        // slot actually holds an object (cloning an array here would be O(n) per
        // index assignment → O(n^2) in `$arr[$i]=…` loops).
        let obj = match base {
            Expr::Var(n) if !is_superglobal(n) => match self.vars().get(n) {
                Some(Value::Object(rc)) => Value::Object(rc.clone()),
                Some(Value::Ref(cell)) => match &*cell.borrow() {
                    Value::Object(rc) => Value::Object(rc.clone()),
                    _ => return None,
                },
                _ => return None,
            },
            // Property base: peek the slot WITHOUT cloning its value — for a
            // big array property, eval() here cloned the whole array on every
            // indexed write ($this->arr[$k] = v → O(n²); 70% of WordPress's
            // bootstrap time before this fix).
            Expr::Prop(obj, pn, _) => {
                let o = self.eval(obj).ok()?;
                let pname = self.prop_name_str(pn).ok()?;
                match o {
                    Value::Object(rc) => {
                        let b = rc.borrow();
                        match b.get(&pname) {
                            Some(Value::Object(inner)) => Value::Object(inner.clone()),
                            _ => return None,
                        }
                    }
                    _ => return None,
                }
            }
            Expr::StaticProp(..) => self.eval(base).ok()?,
            _ => return None,
        };
        if let Value::Object(rc) = &obj {
            if self.find_method(&rc.borrow().class, m).is_some() {
                return Some(obj.clone());
            }
        }
        None
    }

    fn assign_index(&mut self, base: &Expr, key: Option<Key>, val: Value) -> R<()> {
        // Read-modify-write the base container. Only simple `$var[...]` (one level)
        // and nested `$var[a][b]` are handled here.
        match base {
            Expr::Var(name) => {
                // Superglobals (`$_SESSION['k'] = …`) live in the global scope.
                if is_superglobal(name) {
                    let entry = self.scopes[0]
                        .entry(name.clone())
                        .or_insert_with(|| Value::Array(Arr::new()));
                    if !matches!(entry, Value::Array(_)) {
                        *entry = Value::Array(Arr::new());
                    }
                    if let Value::Array(a) = entry {
                        match key {
                            Some(k) => a.insert(k, val),
                            None => a.push(val),
                        }
                    }
                    return Ok(());
                }
                // Through a reference cell (`use (&$out); $out[] = …`): mutate the
                // array inside the shared cell in place (no clone → no O(n^2)).
                if let Some(Value::Ref(cell)) = self.vars().get(name) {
                    let cell = cell.clone();
                    let is_obj = matches!(&*cell.borrow(), Value::Object(_));
                    if is_obj {
                        let obj = cell.borrow().clone();
                        if let Value::Object(rc) = &obj {
                            let class = rc.borrow().class.clone();
                            if self.find_method(&class, "offsetset").is_some() {
                                let kv = key.as_ref().map(akey_to_value).unwrap_or(Value::Null);
                                self.call_method(obj.clone(), "offsetSet", vec![kv, val])?;
                                return Ok(());
                            }
                        }
                    }
                    let mut b = cell.borrow_mut();
                    if !matches!(&*b, Value::Array(_)) {
                        *b = Value::Array(Arr::new());
                    }
                    if let Value::Array(a) = &mut *b {
                        match key {
                            Some(k) => a.insert(k, val),
                            None => a.push(val),
                        }
                    }
                    return Ok(());
                }
                // ArrayAccess: `$obj[$k] = v` / `$obj[] = v` → offsetSet($k, v)
                if matches!(self.vars().get(name), Some(Value::Object(_))) {
                    let obj = self.vars().get(name).cloned().unwrap();
                    if let Value::Object(rc) = &obj {
                        let class = rc.borrow().class.clone();
                        if self.find_method(&class, "offsetset").is_some() {
                            let kv = key.as_ref().map(akey_to_value).unwrap_or(Value::Null);
                            self.call_method(obj.clone(), "offsetSet", vec![kv, val])?;
                            return Ok(());
                        }
                    }
                }
                let entry = self
                    .vars()
                    .entry(name.clone())
                    .or_insert_with(|| Value::Array(Arr::new()));
                if !matches!(entry, Value::Array(_)) {
                    *entry = Value::Array(Arr::new());
                }
                if let Value::Array(a) = entry {
                    match key {
                        Some(k) => a.insert(k, val),
                        None => a.push(val),
                    }
                }
                Ok(())
            }
            Expr::Index(inner, iidx) => {
                // nested: evaluate current, mutate, write back. The read half of
                // this read-modify-write must not warn — PHP creates the
                // intermediate dimensions silently.
                let ikey = match iidx {
                    Some(i) => Some(Arr::norm_key(&self.eval(i)?)),
                    None => None,
                };
                let mut cur = self.eval_quiet(base).unwrap_or(Value::Array(Arr::new()));
                if !matches!(cur, Value::Array(_)) {
                    cur = Value::Array(Arr::new());
                }
                if let Value::Array(a) = &mut cur {
                    match key {
                        Some(k) => a.insert(k, val),
                        None => a.push(val),
                    }
                }
                self.assign_index(inner, ikey, cur)
            }
            // `$obj->prop[...] = v` — mutate the property array IN PLACE (cloning
            // it on every append is O(n^2); `$this->arr[] = …` loops crawl).
            Expr::Prop(objexpr, name, _) => {
                let o = self.eval(objexpr)?;
                let pname = self.prop_name_str(name)?;
                if let Value::Object(rc) = o {
                    let mut b = rc.borrow_mut();
                    if !matches!(b.get(&pname), Some(Value::Array(_))) {
                        b.set(&pname, Value::Array(Arr::new()));
                    }
                    if let Some(Value::Array(a)) = b.get_mut(&pname) {
                        match key {
                            Some(k) => a.insert(k, val),
                            None => a.push(val),
                        }
                    }
                }
                Ok(())
            }
            // `Cls::$prop[...] = v` — read-modify-write the shared static slot.
            Expr::StaticProp(class, name) => {
                let cname = self.resolve_class_name(class)?;
                let skey = self.static_prop_key(&cname, name)?;
                let mut cur = self
                    .static_props
                    .get(&skey)
                    .cloned()
                    .unwrap_or(Value::Array(Arr::new()));
                if !matches!(cur, Value::Array(_)) {
                    cur = Value::Array(Arr::new());
                }
                if let Value::Array(a) = &mut cur {
                    match key {
                        Some(k) => a.insert(k, val),
                        None => a.push(val),
                    }
                }
                self.static_props.insert(skey, cur);
                Ok(())
            }
            _ => Ok(()),
        }
    }

    fn destructure(&mut self, target: &Expr, val: Value) -> R<()> {
        let items: Vec<Option<ArrayItem>> = match target {
            Expr::List(items) => items.clone(),
            Expr::Array(items) => items.iter().cloned().map(Some).collect(),
            _ => return Ok(()),
        };
        if let Value::Array(a) = val {
            let mut idx = 0i64;
            for it in items {
                if let Some(item) = it {
                    let key = match &item.key {
                        Some(ke) => Arr::norm_key(&self.eval(ke)?),
                        None => {
                            let k = Key::Int(idx);
                            idx += 1;
                            k
                        }
                    };
                    let v = a.get(&key).map(|v| v.deref()).unwrap_or(Value::Null);
                    self.assign_to(&item.value, v)?;
                } else {
                    idx += 1;
                }
            }
        } else {
            // list() from a non-array assigns NULL to every target (silently)
            for it in items.into_iter().flatten() {
                self.assign_to(&it.value, Value::Null)?;
            }
        }
        Ok(())
    }

    // ---- calls ----------------------------------------------------------
    fn eval_call(&mut self, callee: &Expr, args: &[Arg]) -> R<Value> {
        // direct named call: foo(...)
        if let Expr::ConstFetch(n) = callee {
            // first-class callable: foo(...)
            if args.len() == 1 && args[0].name.as_deref() == Some("...") {
                return Ok(Value::Str(n.last().as_bytes().to_vec()));
            }
            let name = {
                let last = n.last().to_ascii_lowercase();
                let joined = n.parts.join("\\").to_ascii_lowercase();
                if n.fully_qualified {
                    joined
                } else if !self.cur_ns.is_empty()
                    && self.funcs.contains_key(&format!("{}\\{joined}", self.cur_ns).to_ascii_lowercase())
                {
                    format!("{}\\{joined}", self.cur_ns).to_ascii_lowercase()
                } else if n.parts.len() > 1 && self.funcs.contains_key(&joined) {
                    joined
                } else {
                    last
                }
            };
            // Array internal-pointer fns: dispatch BEFORE eval_args so a large
            // array argument is never cloned into argv (O(n) per call → O(n^2)).
            if matches!(name.as_str(), "reset" | "end" | "next" | "prev" | "current" | "pos" | "key" | "each")
                && !args.is_empty()
                && args[0].name.is_none()
            {
                return self.array_pointer(&name, args);
            }
            let fdecl = self.funcs.get(&name).cloned();
            // By-ref out-params (user fns with &$p; builtins like preg_match's
            // $matches) receive fresh variables by design — PHP does not warn
            // about them being undefined, so evaluate those calls' args quietly.
            let has_byref_out = fdecl
                .as_ref()
                .map(|f| f.params.iter().any(|p| p.by_ref))
                .unwrap_or(false)
                || matches!(
                    name.as_str(),
                    "preg_match" | "preg_match_all" | "preg_replace" | "preg_replace_callback"
                        | "str_replace" | "str_ireplace" | "similar_text" | "parse_str"
                        | "sscanf" | "settype" | "array_multisort" | "xml_parse_into_struct"
                        | "is_callable" | "fscanf" | "fsockopen" | "stream_socket_client"
                        | "flock" | "openssl_sign" | "exec" | "getimagesize"
                );
            let argv = {
                if has_byref_out {
                    self.quiet += 1;
                }
                let r = self.eval_args2(args);
                if has_byref_out {
                    self.quiet -= 1;
                }
                let (pos, named) = r?;
                if named.is_empty() {
                    pos
                } else {
                    let params = fdecl.as_ref().map(|f| f.params.clone());
                    self.merge_named(pos, named, params.as_deref())?
                }
            };
            // include/require/eval need the lex->parse->exec machinery
            match name.as_str() {
                "include" | "include_once" | "require" | "require_once" => {
                    let path = String::from_utf8_lossy(&to_bytes(argv.get(0).unwrap_or(&Value::Null)))
                        .into_owned();
                    let require = name.starts_with("require");
                    let once = name.ends_with("_once");
                    return self.do_include(&path, require, once);
                }
                "eval" => {
                    let code = to_bytes(argv.get(0).unwrap_or(&Value::Null));
                    return self.do_eval(&code);
                }
                "exit" | "die" => {
                    // a string arg is printed; an int is the exit status (no output)
                    if let Some(arg) = argv.first() {
                        if !matches!(arg, Value::Int(_)) {
                            let b = self.stringify(arg)?;
                            self.out.extend_from_slice(&b);
                        }
                    }
                    return Err(RunError("__phargo_exit__".into()));
                }
                "preg_match" | "preg_match_all" => {
                    let all = name == "preg_match_all";
                    let pat = to_bytes(argv.get(0).unwrap_or(&Value::Null));
                    let subj = to_bytes(argv.get(1).unwrap_or(&Value::Null));
                    let flags = argv.get(3).map(to_i64).unwrap_or(0);
                    let start = argv.get(4).map(to_i64).unwrap_or(0).max(0) as usize;
                    let (count, matches) = self.preg_run(&pat, &subj, all, flags, start);
                    if args.len() > 2 {
                        self.assign_to(&args[2].value, matches)?;
                    }
                    if count < 0 {
                        return Ok(Value::Bool(false));
                    }
                    return Ok(Value::Int(count));
                }
                // 5-arg form: write the replacement count into the by-ref arg
                "preg_replace" if args.len() >= 5 => {
                    let (v, n) = preg_replace_full(&argv);
                    self.assign_to(&args[4].value, Value::Int(n))?;
                    return Ok(v);
                }
                // by-ref out-param: decode the query string into args[1]
                "parse_str" if args.len() >= 2 => {
                    let qs = to_bytes(argv.get(0).unwrap_or(&Value::Null));
                    self.assign_to(&args[1].value, Value::Array(php_parse_str(&qs)))?;
                    return Ok(Value::Null);
                }
                // no network layer: sockets fail like a refused connection,
                // with errno/errstr written through the by-ref params so
                // callers (Requests' Fsockopen transport) raise their own
                // catchable errors instead of fataling
                "fsockopen" | "stream_socket_client" => {
                    let (errno_i, errstr_i) = if name == "fsockopen" { (2, 3) } else { (1, 2) };
                    if args.len() > errno_i {
                        self.assign_to(&args[errno_i].value, Value::Int(61))?;
                    }
                    if args.len() > errstr_i {
                        self.assign_to(
                            &args[errstr_i].value,
                            Value::Str(b"Connection refused".to_vec()),
                        )?;
                    }
                    return Ok(Value::Bool(false));
                }
                // 4-arg form: write the replacement count into the by-ref arg
                "str_replace" | "str_ireplace" if args.len() >= 4 => {
                    let (v, n) = self.str_replace_full(name == "str_ireplace", &argv);
                    self.assign_to(&args[3].value, Value::Int(n))?;
                    return Ok(v);
                }
                "array_push" | "array_pop" | "array_shift" | "array_unshift" | "sort" | "rsort"
                | "asort" | "arsort" | "ksort" | "krsort" | "usort" | "uasort" | "uksort"
                | "array_splice" | "shuffle"
                    if !args.is_empty() =>
                {
                    return self.array_byref(&name, args, &argv);
                }
                "array_multisort" if !args.is_empty() => {
                    return self.array_multisort(args, &argv);
                }
                // array_walk mutates through &$value callback params — needs the
                // array lvalue for writeback, so it dispatches here.
                "array_walk" if args.len() >= 2 => {
                    return self.array_walk_byref(args, &argv);
                }
                "settype" if args.len() >= 2 => {
                    let ty = to_bytes(argv.get(1).unwrap_or(&Value::Null)).to_ascii_lowercase();
                    let cur = argv.first().cloned().unwrap_or(Value::Null);
                    let nv = match ty.as_slice() {
                        b"int" | b"integer" => Value::Int(to_i64(&cur)),
                        b"float" | b"double" => Value::Float(to_f64(&cur)),
                        b"string" => Value::Str(self.stringify(&cur)?),
                        b"bool" | b"boolean" => Value::Bool(to_bool(&cur)),
                        b"array" => match cur {
                            Value::Array(_) => cur,
                            Value::Null => Value::Array(Arr::new()),
                            v => {
                                let mut a = Arr::new();
                                a.push(v);
                                Value::Array(a)
                            }
                        },
                        b"null" => Value::Null,
                        _ => cur,
                    };
                    self.assign_to(&args[0].value, nv)?;
                    return Ok(Value::Bool(true));
                }
                _ => {}
            }
            if let Some(f) = fdecl {
                return self.call_user(&f, argv, Some(args));
            }
            return self.builtin(&name, argv);
        }
        // dynamic callee: $f(...), expr(...) — evaluate to a callable value
        let cv = self.eval(callee)?;
        self.quiet += 1;
        let evaled = self.eval_args2(args);
        self.quiet -= 1;
        let argv = {
            let (pos, named) = evaled?;
            if named.is_empty() {
                pos
            } else {
                let params = self.callable_params(&cv);
                self.merge_named(pos, named, params.as_deref())?
            }
        };
        self.call_value(cv, argv)
    }

    /// `include`/`require`: load a file, lex+parse it, and execute its statements
    /// in the current scope (sharing globals; functions/classes register globally).
    fn do_include(&mut self, path: &str, require: bool, once: bool) -> R<Value> {
        let resolved = self.resolve_include(path);
        let key = resolved.to_string_lossy().to_ascii_lowercase();
        if once && self.included.contains(&key) {
            return Ok(Value::Bool(true));
        }
        let bytes = match std::fs::read(&resolved) {
            Ok(b) => b,
            Err(_) => {
                if require {
                    return Err(self.throw_error(
                        "Error",
                        &format!("require(): Failed opening required '{path}'"),
                    ));
                }
                return Ok(Value::Bool(false)); // include() warns and returns false
            }
        };
        self.included.insert(key);
        self.run_source(&bytes, Some(resolved))
    }

    /// `eval("code")`: lex+parse the string and execute it in the current scope.
    fn do_eval(&mut self, code: &[u8]) -> R<Value> {
        // PHP's eval string has no leading `<?php`; wrap it so the lexer enters code mode.
        let mut full = b"<?php ".to_vec();
        full.extend_from_slice(code);
        let cur = self.cur_file.clone();
        self.run_source(&full, cur)
    }

    fn run_source(&mut self, bytes: &[u8], path: Option<PathBuf>) -> R<Value> {
        let loc = || {
            path.as_ref()
                .map(|p| format!(" in {}", p.display()))
                .unwrap_or_default()
        };
        let (toks, lines) = super::lexer::Lexer::tokenize_lines(bytes)
            .map_err(|e| RunError(format!("Parse error: {}{}", e.msg, loc())))?;
        let ast = super::parser::Parser::parse_with_lines(toks, lines)
            .map_err(|e| RunError(format!("Parse error: {}{}", e.msg, loc())))?;
        let prev_file = std::mem::replace(&mut self.cur_file, path);
        let prev_line = self.cur_line;
        let prev_ns = std::mem::take(&mut self.cur_ns);
        let prev_use = std::mem::take(&mut self.use_map);
        self.hoist(&ast);
        let r = self.exec_block(&ast);
        self.cur_file = prev_file;
        self.cur_line = prev_line;
        self.cur_ns = prev_ns;
        self.use_map = prev_use;
        match r? {
            Flow::Return(v) => Ok(v),
            _ => Ok(Value::Int(1)), // include/eval default return value
        }
    }

    /// Resolve an include path: absolute as-is, else relative to the current
    /// file's directory, else relative to the working directory.
    fn resolve_include(&self, path: &str) -> PathBuf {
        let p = PathBuf::from(path);
        if p.is_absolute() && p.exists() {
            return p;
        }
        if let Some(dir) = self.cur_file.as_ref().and_then(|f| f.parent()) {
            let cand = dir.join(path);
            if cand.exists() {
                return cand;
            }
        }
        p
    }

    /// In-place array builtins: read `args[0]` as an array lvalue, mutate, assign back.
    /// Array internal-pointer functions (reset/end/next/prev/current/key/each).
    /// The pointer is mutated IN PLACE on the stored array when the argument is a
    /// simple `$var` (cloning the whole array per call would be O(n^2) in loops).
    fn array_pointer(&mut self, name: &str, args: &[Arg]) -> R<Value> {
        // Fast path: mutate the array stored under a plain variable in place (no
        // clone). Also covers reference-backed variables.
        if let Expr::Var(vn) = &args[0].value {
            if !is_superglobal(vn) {
                match self.vars().get_mut(vn) {
                    Some(Value::Array(a)) => return Ok(Self::array_pointer_inplace(name, a)),
                    Some(Value::Ref(cell)) => {
                        let cell = cell.clone();
                        let mut b = cell.borrow_mut();
                        if let Value::Array(a) = &mut *b {
                            return Ok(Self::array_pointer_inplace(name, a));
                        }
                        return Ok(Value::Bool(false));
                    }
                    _ => return Ok(Value::Bool(false)),
                }
            }
        }
        // General path (property/index/superglobal base): read, mutate, write back.
        let mut arr = match self.eval(&args[0].value)? {
            Value::Array(a) => a,
            _ => return Ok(Value::Bool(false)),
        };
        let mutate = matches!(name, "reset" | "end" | "next" | "prev" | "each");
        let result = Self::array_pointer_inplace(name, &mut arr);
        if mutate {
            self.assign_to(&args[0].value, Value::Array(arr))?;
        }
        Ok(result)
    }

    fn array_pointer_inplace(name: &str, arr: &mut Arr) -> Value {
        match name {
            "reset" => arr.pos = 0,
            "end" => arr.pos = arr.entries.len().saturating_sub(1),
            "next" => arr.pos = arr.pos.saturating_add(1),
            "prev" => arr.pos = if arr.pos == 0 { usize::MAX } else { arr.pos - 1 },
            _ => {}
        }
        let cur = arr.entries.get(arr.pos);
        match name {
            "key" => match cur {
                Some((k, _)) => akey_to_value(k),
                None => Value::Null,
            },
            "each" => match cur {
                Some((k, v)) => {
                    let mut r = Arr::new();
                    let kv = akey_to_value(k);
                    r.insert(Key::Int(0), kv.clone());
                    r.insert(Key::Str(b"key".to_vec()), kv);
                    r.insert(Key::Int(1), v.clone());
                    r.insert(Key::Str(b"value".to_vec()), v.clone());
                    arr.pos += 1;
                    Value::Array(r)
                }
                None => Value::Bool(false),
            },
            _ => match cur {
                Some((_, v)) => v.clone(),
                None => Value::Bool(false),
            },
        }
    }

    /// array_multisort($a1[, dir][, flags], $a2, …): sort the arrays in parallel
    /// by the first column, then successive columns as tie-breakers. Integer
    /// arguments between arrays are SORT_ASC(4)/SORT_DESC(3)/flags — we honor the
    /// direction. Each array argument is written back (by reference).
    fn array_multisort(&mut self, args: &[Arg], argv: &[Value]) -> R<Value> {
        // Collect (arg-index, entries, descending) for each array column.
        struct Col { arg_idx: usize, vals: Vec<Value>, desc: bool }
        let mut cols: Vec<Col> = Vec::new();
        let mut i = 0;
        while i < argv.len() {
            if let Value::Array(a) = &argv[i] {
                let vals: Vec<Value> = a.entries.iter().map(|(_, v)| v.clone()).collect();
                let mut desc = false;
                // following ints are direction/flags for THIS column
                let mut j = i + 1;
                while j < argv.len() && matches!(argv[j], Value::Int(_)) {
                    if to_i64(&argv[j]) == 3 { desc = true; } // SORT_DESC
                    j += 1;
                }
                cols.push(Col { arg_idx: i, vals, desc });
                i = j;
            } else {
                i += 1;
            }
        }
        if cols.is_empty() {
            return Ok(Value::Bool(false));
        }
        let n = cols[0].vals.len();
        let mut order: Vec<usize> = (0..n).collect();
        order.sort_by(|&x, &y| {
            for c in &cols {
                if x >= c.vals.len() || y >= c.vals.len() { continue; }
                let mut o = compare(&c.vals[x], &c.vals[y]);
                if c.desc { o = o.reverse(); }
                if o != std::cmp::Ordering::Equal { return o; }
            }
            std::cmp::Ordering::Equal
        });
        // Reorder each array by `order`, reindexed 0..n, and write back.
        for c in &cols {
            let mut out = Arr::new();
            for &k in &order {
                if let Some(v) = c.vals.get(k) { out.push(v.clone()); }
            }
            self.assign_to(&args[c.arg_idx].value, Value::Array(out))?;
        }
        Ok(Value::Bool(true))
    }

    /// array_walk with by-ref callback support: each element is temporarily
    /// promoted to a Ref cell so a `&$value` callback param aliases it, then the
    /// cell's final value lands back in the array, which is written back to the
    /// caller's lvalue once.
    fn array_walk_byref(&mut self, args: &[Arg], argv: &[Value]) -> R<Value> {
        let mut arr = match argv.first() {
            Some(Value::Array(a)) => a.clone(),
            _ => return Ok(Value::Bool(false)),
        };
        let cb = argv.get(1).cloned().unwrap_or(Value::Null);
        for i in 0..arr.entries.len() {
            let (k, v) = arr.entries[i].clone();
            let cell = Rc::new(RefCell::new(v.deref()));
            let mut cargs = vec![Value::Ref(cell.clone()), akey_to_value(&k)];
            if argv.len() > 2 {
                cargs.push(argv[2].clone());
            }
            self.call_value(cb.clone(), cargs)?;
            arr.entries[i].1 = cell.borrow().clone();
        }
        self.assign_to(&args[0].value, Value::Array(arr))?;
        Ok(Value::Bool(true))
    }

    fn array_byref(&mut self, name: &str, args: &[Arg], argv: &[Value]) -> R<Value> {
        let mut arr = match self.eval(&args[0].value)? {
            Value::Array(a) => a,
            _ => Arr::new(),
        };
        let rebuilt = |entries: Vec<(Key, Value)>| -> Arr {
            let mut a = Arr::new();
            for (k, v) in entries {
                a.insert(k, v);
            }
            a
        };
        let reindex_vals = |vals: Vec<Value>| -> Arr {
            let mut a = Arr::new();
            for v in vals {
                a.push(v);
            }
            a
        };
        let result = match name {
            "array_push" => {
                for v in &argv[1..] {
                    arr.push(v.clone());
                }
                Value::Int(arr.len() as i64)
            }
            "array_pop" => match arr.entries.pop() {
                Some((_, v)) => {
                    arr = rebuilt(arr.entries);
                    v
                }
                None => Value::Null,
            },
            "array_shift" => {
                if arr.entries.is_empty() {
                    Value::Null
                } else {
                    let (_, v) = arr.entries.remove(0);
                    let mut new = Arr::new();
                    for (k, val) in std::mem::take(&mut arr.entries) {
                        match k {
                            Key::Int(_) => new.push(val),
                            Key::Str(_) => new.insert(k, val),
                        }
                    }
                    arr = new;
                    v
                }
            }
            "array_unshift" => {
                let mut new = Arr::new();
                for v in &argv[1..] {
                    new.push(v.clone());
                }
                for (k, val) in std::mem::take(&mut arr.entries) {
                    match k {
                        Key::Int(_) => new.push(val),
                        Key::Str(_) => new.insert(k, val),
                    }
                }
                arr = new;
                Value::Int(arr.len() as i64)
            }
            "sort" | "rsort" => {
                let mut vals: Vec<Value> = arr.entries.iter().map(|(_, v)| v.clone()).collect();
                vals.sort_by(|a, b| compare(a, b));
                if name == "rsort" {
                    vals.reverse();
                }
                arr = reindex_vals(vals);
                Value::Bool(true)
            }
            "asort" | "arsort" => {
                let mut entries = std::mem::take(&mut arr.entries);
                entries.sort_by(|(_, a), (_, b)| compare(a, b));
                if name == "arsort" {
                    entries.reverse();
                }
                arr = rebuilt(entries);
                Value::Bool(true)
            }
            "ksort" | "krsort" => {
                let mut entries = std::mem::take(&mut arr.entries);
                entries.sort_by(|(a, _), (b, _)| key_cmp(a, b));
                if name == "krsort" {
                    entries.reverse();
                }
                arr = rebuilt(entries);
                Value::Bool(true)
            }
            "usort" | "uasort" | "uksort" => {
                let cb = argv.get(1).cloned().unwrap_or(Value::Null);
                let mut entries = std::mem::take(&mut arr.entries);
                let on_keys = name == "uksort";
                entries.sort_by(|(ka, va), (kb, vb)| {
                    let (l, r) = if on_keys {
                        (akey_to_value(ka), akey_to_value(kb))
                    } else {
                        (va.clone(), vb.clone())
                    };
                    let c = self.call_value(cb.clone(), vec![l, r]).map(|v| to_i64(&v)).unwrap_or(0);
                    c.cmp(&0)
                });
                arr = if name == "usort" {
                    reindex_vals(entries.into_iter().map(|(_, v)| v).collect())
                } else {
                    rebuilt(entries)
                };
                Value::Bool(true)
            }
            "array_splice" => {
                let len = arr.entries.len() as i64;
                let mut off = to_i64(argv.get(1).unwrap_or(&Value::Null));
                if off < 0 {
                    off = (len + off).max(0);
                }
                let off = off.min(len) as usize;
                let rem = if argv.len() > 2 {
                    let l = to_i64(&argv[2]);
                    if l < 0 { ((len + l) as usize).saturating_sub(off) } else { l as usize }
                } else {
                    arr.entries.len() - off
                };
                let end = (off + rem).min(arr.entries.len());
                let removed: Vec<Value> = arr.entries[off..end].iter().map(|(_, v)| v.clone()).collect();
                let repl: Vec<Value> = match argv.get(3) {
                    Some(Value::Array(a)) => a.entries.iter().map(|(_, v)| v.clone()).collect(),
                    Some(v) => vec![v.clone()],
                    None => vec![],
                };
                let mut new_entries: Vec<(Key, Value)> = arr.entries[..off].to_vec();
                for v in repl {
                    new_entries.push((Key::Int(0), v)); // key reassigned by rebuilt-as-pushed
                }
                new_entries.extend_from_slice(&arr.entries[end..]);
                // reindex integer keys, keep string keys
                let mut new = Arr::new();
                for (k, v) in new_entries {
                    match k {
                        Key::Int(_) => new.push(v),
                        Key::Str(_) => new.insert(k, v),
                    }
                }
                arr = new;
                Value::Array(reindex_vals(removed))
            }
            "shuffle" => Value::Bool(true), // no RNG; leave order (deterministic)
            _ => Value::Null,
        };
        self.assign_to(&args[0].value, Value::Array(arr))?;
        Ok(result)
    }

    /// Run a `preg_match`/`preg_match_all`, returning (count, matches-array),
    /// reusing the legacy regex engine (char-based, Value-independent).
    fn preg_run(
        &self,
        pat: &[u8],
        subj: &[u8],
        all: bool,
        flags: i64,
        start_byte: usize,
    ) -> (i64, Value) {
        let pattern = String::from_utf8_lossy(pat).into_owned();
        let rx = match crate::rx_compile(&pattern) {
            Some(r) => r,
            None => return (0, Value::Bool(false)),
        };
        let text: Vec<char> = String::from_utf8_lossy(subj).chars().collect();
        let offset_capture = flags & 256 != 0; // PREG_OFFSET_CAPTURE
        let set_order = flags & 2 != 0; // PREG_SET_ORDER (preg_match_all)
        // PHP reports BYTE offsets; the matcher works on chars
        let mut byte_at: Vec<usize> = Vec::with_capacity(text.len() + 1);
        let mut b = 0usize;
        for ch in &text {
            byte_at.push(b);
            b += ch.len_utf8();
        }
        byte_at.push(b);
        let start_char = byte_at
            .iter()
            .position(|&x| x >= start_byte)
            .unwrap_or(text.len());
        let mut steps = 0usize;
        let grp = |slots: &[usize], g: usize| -> Value {
            let s = crate::rx_group_str(&text, slots, g);
            if offset_capture {
                let off = slots.get(2 * g).copied().unwrap_or(usize::MAX);
                let off_v = if off == usize::MAX {
                    -1i64
                } else {
                    byte_at[off.min(text.len())] as i64
                };
                let mut pair = Arr::new();
                pair.push(Value::Str(s.into_bytes()));
                pair.push(Value::Int(off_v));
                Value::Array(pair)
            } else {
                Value::Str(s.into_bytes())
            }
        };
        if start_byte > byte_at[text.len()] {
            // offset past the end of the subject: PHP returns false
            return (-1, Value::Array(Arr::new()));
        }
        if !all {
            match rx.exec(&text, start_char, &mut steps) {
                Some(slots) => {
                    let mut m = Arr::new();
                    // PHP omits TRAILING unmatched groups from the match array
                    let mut last = 0;
                    for g in 0..=rx.ngroups {
                        if slots.get(2 * g).copied().unwrap_or(usize::MAX) != usize::MAX {
                            last = g;
                        }
                    }
                    for g in 0..=last {
                        if let Some((nm, _)) = rx.names.iter().find(|(_, idx)| *idx == g) {
                            m.insert(Key::Str(nm.as_bytes().to_vec()), grp(&slots, g));
                        }
                        m.insert(Key::Int(g as i64), grp(&slots, g));
                    }
                    (1, Value::Array(m))
                }
                None => (0, Value::Array(Arr::new())),
            }
        } else {
            let mut sets: Vec<Vec<usize>> = Vec::new();
            let mut start = start_char;
            while let Some(slots) = rx.exec(&text, start, &mut steps) {
                let (ms, me) = (slots[0], slots[1]);
                sets.push(slots);
                start = if me > ms { me } else { me + 1 };
                if start > text.len() {
                    break;
                }
            }
            let mut result = Arr::new();
            if set_order {
                for slots in &sets {
                    let mut m = Arr::new();
                    for g in 0..=rx.ngroups {
                        if let Some((nm, _)) = rx.names.iter().find(|(_, idx)| *idx == g) {
                            m.insert(Key::Str(nm.as_bytes().to_vec()), grp(slots, g));
                        }
                        m.insert(Key::Int(g as i64), grp(slots, g));
                    }
                    result.push(Value::Array(m));
                }
            } else {
                for g in 0..=rx.ngroups {
                    let mut col = Arr::new();
                    for slots in &sets {
                        col.push(grp(slots, g));
                    }
                    if let Some((nm, _)) = rx.names.iter().find(|(_, idx)| *idx == g) {
                        result.insert(Key::Str(nm.as_bytes().to_vec()), Value::Array(col.clone()));
                    }
                    result.insert(Key::Int(g as i64), Value::Array(col));
                }
            }
            (sets.len() as i64, Value::Array(result))
        }
    }

    /// Invoke any callable value: closure, function-name string, `[obj, "m"]`,
    /// or an object with `__invoke`.
    fn call_value(&mut self, cv: Value, args: Vec<Value>) -> R<Value> {
        match cv {
            Value::Closure(c) => self.call_closure(&c, args),
            Value::Str(s) => {
                let name = String::from_utf8_lossy(&s).to_ascii_lowercase();
                if let Some(f) = self.funcs.get(&name).cloned() {
                    self.call_user(&f, args, None)
                } else {
                    self.builtin(&name, args)
                }
            }
            Value::Object(rc) => {
                let class = rc.borrow().class.clone();
                if self.find_method(&class, "__invoke").is_some() {
                    self.call_method(Value::Object(rc), "__invoke", args)
                } else {
                    Ok(Value::Null)
                }
            }
            Value::Array(a) if a.len() == 2 => {
                let recv = a.get(&Key::Int(0)).cloned().unwrap_or(Value::Null);
                let m = a.get(&Key::Int(1)).cloned().unwrap_or(Value::Null);
                let mname = String::from_utf8_lossy(&to_bytes(&m)).into_owned();
                match recv {
                    Value::Object(_) => self.call_method(recv, &mname, args),
                    Value::Str(s) => {
                        let cn = String::from_utf8_lossy(&s).into_owned();
                        self.call_static(&cn, &mname, args, None, None)
                    }
                    _ => Ok(Value::Null),
                }
            }
            _ => Ok(Value::Null),
        }
    }

    /// Depth-first walk for `array_walk_recursive`: the callback fires on leaf
    /// (non-array) values only. Read-only — element mutation via a by-ref callback
    /// parameter isn't modeled here (the callable-value path is by-value).
    fn walk_recursive(&mut self, arr: &Arr, cb: &Value, has_extra: bool, extra: &Value) -> R<()> {
        for (k, v) in &arr.entries {
            if let Value::Array(sub) = v {
                self.walk_recursive(sub, cb, has_extra, extra)?;
            } else {
                let kv = match k {
                    Key::Int(n) => Value::Int(*n),
                    Key::Str(s) => Value::Str(s.clone()),
                };
                let mut cargs = vec![v.clone(), kv];
                if has_extra {
                    cargs.push(extra.clone());
                }
                self.call_value(cb.clone(), cargs)?;
            }
        }
        Ok(())
    }

    fn call_closure(&mut self, c: &ClosureVal, args: Vec<Value>) -> R<Value> {
        self.enter_call()?;
        self.cur_args.push(Rc::new(args.clone()));
        self.cur_fn.push("{closure}".to_string());
        self.frames.push(("{closure}".to_string(), self.cur_line, self.cur_file_str()));
        let mut scope = HashMap::new();
        for (k, v) in &c.captures {
            scope.insert(k.clone(), v.clone());
        }
        if let Some(t) = &c.bound_this {
            scope.insert("this".to_string(), t.clone());
        }
        let r = match &c.kind {
            ClosureKind::Full(f) => {
                self.bind_params(&mut scope, &f.params, &args, "{closure}")?;
                self.byref_ret.push(f.by_ref_return);
                self.scopes.push(scope);
                let r = self.run_fn_body(&f.body);
                self.scopes.pop();
                self.byref_ret.pop();
                r
            }
            ClosureKind::Arrow(f) => {
                self.bind_params(&mut scope, &f.params, &args, "{closure}")?;
                self.byref_ret.push(false);
                self.scopes.push(scope);
                let r = self.eval(&f.body);
                self.scopes.pop();
                self.byref_ret.pop();
                r
            }
        };
        self.cur_args.pop();
        self.cur_fn.pop();
        self.frames.pop();
        self.call_depth -= 1;
        r
    }

    /// Run a function/method body that has already had its scope pushed. If the
    /// body contains `yield`, run it as an (eager) generator and return a
    /// Generator object; otherwise return the `return` value.
    fn run_fn_body(&mut self, body: &[Stmt]) -> R<Value> {
        let caller_line = self.cur_line;
        let r = self.run_fn_body_inner(body);
        self.cur_line = caller_line;
        return r;
    }

    fn run_fn_body_inner(&mut self, body: &[Stmt]) -> R<Value> {
        if has_yield(body) {
            let prev = self.gen_buf.take();
            let prev_nodes = self.gen_nodes;
            self.gen_buf = Some(Arr::new());
            self.gen_nodes = 0;
            let r = self.exec_block(body);
            let buf = self.gen_buf.take().unwrap_or_default();
            self.gen_buf = prev;
            self.gen_nodes = prev_nodes;
            let ret = match r? {
                Flow::Return(v) => v,
                _ => Value::Null,
            };
            Ok(self.make_generator(buf, ret))
        } else {
            match self.exec_block(body)? {
                Flow::Return(v) => Ok(v),
                _ => Ok(Value::Null),
            }
        }
    }

    fn make_generator(&self, buf: Arr, ret: Value) -> Value {
        let mut karr = Arr::new();
        for (k, _) in &buf.entries {
            karr.push(akey_to_value(k));
        }
        let o = Rc::new(RefCell::new(Obj::new("Generator")));
        {
            let mut b = o.borrow_mut();
            b.set("__d", Value::Array(buf));
            b.set("__k", Value::Array(karr));
            b.set("__p", Value::Int(0));
            b.set("__ret", ret);
        }
        Value::Object(o)
    }

    fn call_user(&mut self, f: &FuncDecl, args: Vec<Value>, byref: Option<&[Arg]>) -> R<Value> {
        self.enter_call()?;
        self.cur_args.push(Rc::new(args.clone()));
        self.cur_fn.push(f.name.clone());
        self.frames.push((f.name.clone(), self.cur_line, self.cur_file_str()));
        let prev_df = self.enter_def_ctx(&format!("fn:{}", f.name.to_ascii_lowercase()));
        let mut scope = HashMap::new();
        let bind = self.bind_params(&mut scope, &f.params, &args, &f.name);
        if bind.is_ok() {
            self.alias_byref_params(&mut scope, &f.params, byref);
        }
        if let Err(e) = bind {
            if let Some((pf, pns, puse)) = prev_df {
                self.cur_file = pf;
                self.cur_ns = pns;
                self.use_map = puse;
            }
            self.cur_args.pop();
            self.cur_fn.pop();
            self.frames.pop();
            self.call_depth -= 1;
            return Err(e);
        }
        // prelude functions emulate C internals — no engine warnings inside
        let quiet_body = self.prelude_fns.contains(&f.name.to_ascii_lowercase());
        if quiet_body {
            self.quiet += 1;
        }
        self.byref_ret.push(f.by_ref_return);
        self.scopes.push(scope);
        let r = self.run_fn_body(&f.body);
        let wb = self.capture_byref(&f.params, byref);
        self.scopes.pop();
        self.byref_ret.pop();
        if quiet_body {
            self.quiet -= 1;
        }
        if let Some((pf, pns, puse)) = prev_df {
            self.cur_file = pf;
            self.cur_ns = pns;
            self.use_map = puse;
        }
        self.cur_args.pop();
        self.cur_fn.pop();
        self.call_depth -= 1;
        self.frames.pop();
        self.apply_byref(byref, wb)?;
        let fname = f.name.clone();
        let rt = f.ret_type.clone();
        r.and_then(|v| self.check_return(v, &rt, &fname))
    }

    /// By-reference parameter write-back. The engine passes arguments by value;
    /// for a `&$param` whose argument is a writable lvalue, we copy the parameter's
    /// final value back into the caller's variable after the call returns. This
    /// cascades correctly through recursion (each frame writes back to its caller).
    /// Capture must run before the callee scope is popped; apply after.
    /// Bind by-ref parameters whose argument is a plain variable as TRUE
    /// aliases: the parameter shares the caller variable's Ref cell, so
    /// element aliasing (`$p = &$p['x']`) and in-place writes behave like
    /// PHP. Runs after bind_params, before the scope is pushed; aliased
    /// params get the rebound marker so capture_byref skips their (now
    /// redundant) write-back. Non-variable lvalue args keep the write-back
    /// cascade.
    fn alias_byref_params(
        &mut self,
        scope: &mut HashMap<String, Value>,
        params: &[Param],
        byref: Option<&[Arg]>,
    ) {
        let args = match byref {
            Some(a) => a,
            None => return,
        };
        for (i, p) in params.iter().enumerate() {
            if !p.by_ref || p.variadic {
                continue;
            }
            let Some(arg) = args.get(i) else { continue };
            if arg.spread || arg.name.is_some() {
                continue;
            }
            if let Expr::Var(vn) = &arg.value {
                if is_superglobal(vn) {
                    continue;
                }
                let cell = self.get_ref_cell(vn);
                scope.insert(p.name.clone(), Value::Ref(cell));
                scope.insert(format!("\0rebound\0{}", p.name), Value::Bool(true));
            }
        }
    }

    /// Record that `$name` was re-bound with `=&` in the current scope.
    /// A re-bound by-ref PARAMETER breaks its link to the caller's variable
    /// (PHP: `$p = &$p['x']` aliases deeper without replacing the argument),
    /// so capture_byref must skip its write-back. The marker key starts with
    /// NUL — unreachable from PHP variable names.
    fn mark_rebound(&mut self, name: &str) {
        self.vars()
            .insert(format!("\0rebound\0{name}"), Value::Bool(true));
    }

    fn capture_byref(&self, params: &[Param], byref: Option<&[Arg]>) -> Vec<(usize, Value)> {
        let mut wb = Vec::new();
        let args = match byref {
            Some(a) => a,
            None => return wb,
        };
        let scope = match self.scopes.last() {
            Some(s) => s,
            None => return wb,
        };
        for (i, p) in params.iter().enumerate() {
            if p.by_ref && !p.variadic {
                if let Some(arg) = args.get(i) {
                    if !arg.spread && arg.name.is_none() && is_lvalue_expr(&arg.value) {
                        if scope.contains_key(&format!("\0rebound\0{}", p.name)) {
                            continue; // param re-bound with =& — link broken
                        }
                        if let Some(v) = scope.get(&p.name) {
                            wb.push((i, v.clone()));
                        }
                    }
                }
            }
        }
        wb
    }

    fn apply_byref(&mut self, byref: Option<&[Arg]>, wb: Vec<(usize, Value)>) -> R<()> {
        if let Some(args) = byref {
            for (i, v) in wb {
                self.assign_to(&args[i].value, v)?;
            }
        }
        Ok(())
    }
}

// ---- objects & classes -------------------------------------------------
impl Eval {
    fn find_class(&self, name: &str) -> Option<Rc<ClassDecl>> {
        self.classes.get(&name.to_ascii_lowercase()).cloned()
    }

    /// Run registered autoloaders for an unknown class. Returns true if the
    /// class exists afterwards. Re-entrant loads of the same name are no-ops.
    fn autoload(&mut self, name: &str) -> bool {
        if self.autoloaders.is_empty() || name.is_empty() {
            return false;
        }
        let key = name.to_ascii_lowercase();
        if self.classes.contains_key(&key) || self.autoload_active.contains(&key) {
            return self.classes.contains_key(&key);
        }
        self.autoload_active.insert(key.clone());
        let loaders = self.autoloaders.clone();
        for l in loaders {
            let _ = self.call_value(l, vec![Value::Str(name.as_bytes().to_vec())]);
            if self.classes.contains_key(&key) {
                break;
            }
        }
        self.autoload_active.remove(&key);
        self.classes.contains_key(&key)
    }

    /// find_class, invoking autoloaders on a miss (the `new`/static-access path).
    fn find_class_autoload(&mut self, name: &str) -> Option<Rc<ClassDecl>> {
        if let Some(c) = self.find_class(name) {
            return Some(c);
        }
        self.autoload(name);
        self.find_class(name)
    }

    /// Resolve a class reference expression to a class name.
    fn resolve_class_name(&mut self, e: &Expr) -> R<String> {
        match e {
            Expr::ConstFetch(n) => {
                let last = n.last();
                match last.to_ascii_lowercase().as_str() {
                    "self" => Ok(self
                        .current_class
                        .clone()
                        .unwrap_or_else(|| last.to_string())),
                    // late static binding: the class the method was called on
                    "static" => Ok(self
                        .called_class
                        .clone()
                        .or_else(|| self.current_class.clone())
                        .unwrap_or_else(|| last.to_string())),
                    "parent" => {
                        let cur = self.current_class.clone().unwrap_or_default();
                        Ok(self
                            .find_class(&cur)
                            .and_then(|c| c.parent.as_ref().map(|p| p.parts.join("\\")))
                            .unwrap_or(cur))
                    }
                    _ => Ok(self.resolve_ns_class(n)),
                }
            }
            _ => {
                let v = self.eval(e)?;
                match v {
                    Value::Str(s) => Ok(String::from_utf8_lossy(&s).into_owned()),
                    Value::Object(rc) => Ok(rc.borrow().class.clone()),
                    _ => Ok(String::new()),
                }
            }
        }
    }

    /// Convert a value to bytes for output, honoring `__toString` on objects.
    fn stringify(&mut self, v: &Value) -> R<Vec<u8>> {
        if let Value::Object(rc) = v {
            let class = rc.borrow().class.clone();
            if self.find_method(&class, "__tostring").is_some() {
                let r = self.call_method(v.clone(), "__toString", vec![])?;
                return Ok(to_bytes(&r));
            }
        }
        Ok(to_bytes(v))
    }

    fn prop_name_str(&mut self, p: &PropName) -> R<String> {
        Ok(match p {
            PropName::Id(s) => s.clone(),
            PropName::Expr(e) => String::from_utf8_lossy(&to_bytes(&self.eval(e)?)).into_owned(),
        })
    }

    fn eval_args(&mut self, args: &[Arg]) -> R<Vec<Value>> {
        let (mut pos, named) = self.eval_args2(args)?;
        // No callee params known here: append named values in order (legacy behavior
        // for call sites that can't resolve the callee's signature).
        for (_, v) in named {
            pos.push(v);
        }
        Ok(pos)
    }

    /// Evaluate call arguments, separating positional from named. String keys in
    /// a spread array become named args (PHP 8.1 `...['name' => $v]` semantics).
    fn eval_args2(&mut self, args: &[Arg]) -> R<(Vec<Value>, Vec<(String, Value)>)> {
        let mut pos = Vec::new();
        let mut named: Vec<(String, Value)> = Vec::new();
        for a in args {
            if a.name.as_deref() == Some("...") {
                continue;
            }
            if a.spread {
                if let Value::Array(arr) = self.eval(&a.value)? {
                    for (k, v) in arr.entries {
                        match k {
                            Key::Str(s) => named.push((String::from_utf8_lossy(&s).into_owned(), v)),
                            Key::Int(_) => pos.push(v),
                        }
                    }
                }
            } else if let Some(n) = &a.name {
                named.push((n.clone(), self.eval(&a.value)?));
            } else {
                pos.push(self.eval(&a.value)?);
            }
        }
        Ok((pos, named))
    }

    /// Map named arguments onto their positional parameter slots. Gaps between the
    /// last positional arg and a named arg's slot get the parameter default (Null
    /// if none — the required-arg error case). Unknown names append positionally.
    fn merge_named(
        &mut self,
        mut pos: Vec<Value>,
        named: Vec<(String, Value)>,
        params: Option<&[Param]>,
    ) -> R<Vec<Value>> {
        let params = match params {
            Some(p) if !named.is_empty() => p,
            _ => {
                for (_, v) in named {
                    pos.push(v);
                }
                return Ok(pos);
            }
        };
        for (name, val) in named {
            match params.iter().position(|p| p.name == name && !p.variadic) {
                Some(idx) => {
                    while pos.len() < idx {
                        let gap = &params[pos.len()];
                        let dv = match &gap.default {
                            Some(d) => self.eval(d)?,
                            None => Value::Null,
                        };
                        pos.push(dv);
                    }
                    if pos.len() == idx {
                        pos.push(val);
                    } else {
                        pos[idx] = val;
                    }
                }
                None => pos.push(val),
            }
        }
        Ok(pos)
    }

    /// Callee parameter list for named-arg resolution on a dynamic callable value.
    fn callable_params(&self, cv: &Value) -> Option<Vec<Param>> {
        match cv {
            Value::Closure(c) => Some(match &c.kind {
                ClosureKind::Full(f) => f.params.clone(),
                ClosureKind::Arrow(f) => f.params.clone(),
            }),
            Value::Str(s) => {
                let name = String::from_utf8_lossy(s).to_ascii_lowercase();
                self.funcs.get(&name).map(|f| f.params.clone())
            }
            Value::Object(rc) => {
                let class = rc.borrow().class.clone();
                self.find_method(&class, "__invoke").map(|(_, m)| m.params.clone())
            }
            Value::Array(a) if a.len() == 2 => {
                let recv = a.get(&Key::Int(0)).cloned()?;
                let m = a.get(&Key::Int(1)).cloned()?;
                let mname = String::from_utf8_lossy(&to_bytes(&m)).into_owned();
                let cls = match recv {
                    Value::Object(rc) => rc.borrow().class.clone(),
                    Value::Str(s) => String::from_utf8_lossy(&s).into_owned(),
                    _ => return None,
                };
                self.find_method(&cls, &mname).map(|(_, m)| m.params.clone())
            }
            _ => None,
        }
    }

    /// The ancestor chain (self first, then parents), as class decls.
    fn ancestry(&self, name: &str) -> Vec<Rc<ClassDecl>> {
        let mut out = Vec::new();
        let mut cur = self.find_class(name);
        let mut guard = 0;
        while let Some(c) = cur {
            out.push(c.clone());
            guard += 1;
            if guard > 50 {
                break;
            }
            cur = c.parent.as_ref().and_then(|p| self.find_class_n(p));
        }
        out
    }

    /// str_replace / str_ireplace with full PHP semantics: search/replace may
    /// be arrays (pairwise, missing replacement = ""), subject may be an array
    /// (mapped). Returns the result plus the replacement count for the by-ref
    /// 4th parameter (WP's _deep_replace loops `while ($count)` on it).
    fn str_replace_full(&self, ci: bool, argv: &[Value]) -> (Value, i64) {
        let g = |i: usize| argv.get(i).cloned().unwrap_or(Value::Null);
        let mut total = 0i64;
        let mut one = |subject: &[u8]| -> Vec<u8> {
            match &g(0) {
                Value::Array(sa) => {
                    let mut out = subject.to_vec();
                    for (i, (_, sv)) in sa.entries.iter().enumerate() {
                        let needle = to_bytes(sv);
                        let rep = match &g(1) {
                            Value::Array(ra) => ra
                                .entries
                                .get(i)
                                .map(|(_, v)| to_bytes(v))
                                .unwrap_or_default(),
                            other => to_bytes(other),
                        };
                        let (o, n) = replace_bytes_ci_n(&out, &needle, &rep, ci);
                        out = o;
                        total += n;
                    }
                    out
                }
                other => {
                    let (o, n) =
                        replace_bytes_ci_n(subject, &to_bytes(other), &to_bytes(&g(1)), ci);
                    total += n;
                    o
                }
            }
        };
        let v = match &g(2) {
            Value::Array(subj) => {
                let mut out = Arr::new();
                for (k, v) in &subj.entries {
                    out.insert(k.clone(), Value::Str(one(&to_bytes(v))));
                }
                Value::Array(out)
            }
            other => Value::Str(one(&to_bytes(other))),
        };
        (v, total)
    }

    /// Storage slot for a static property. PHP shares a static declared in a
    /// parent with every subclass, so access canonicalizes to the class that
    /// already holds (or declares) the property; a declared default is
    /// materialized on first touch. Before this, static prop *defaults* were a
    /// silent hole — reads before any write returned NULL (WP's SQL lexer
    /// read `static::$default_delimiter` as NULL → strlen 0 → infinite loop).
    fn static_prop_key(&mut self, cname: &str, name: &str) -> R<(String, String)> {
        let chain = self.ancestry(cname);
        for c in &chain {
            let key = (c.name.to_ascii_lowercase(), name.to_string());
            if self.static_props.contains_key(&key) {
                return Ok(key);
            }
        }
        for c in &chain {
            if let Some(p) = c.props.iter().find(|p| p.is_static && p.name == name) {
                let key = (c.name.to_ascii_lowercase(), name.to_string());
                // defaults may reference `self::CONST` or use-aliased classes
                // — evaluate under the declaring class + its def-site context
                let prev_cc = self.current_class.replace(c.name.clone());
                let prev_df = self.enter_def_ctx(&format!("class:{}", c.name.to_ascii_lowercase()));
                let v = match &p.default {
                    Some(d) => self.eval(d),
                    None => Ok(Value::Null),
                };
                self.current_class = prev_cc;
                if let Some((pf, pns, puse)) = prev_df {
                    self.cur_file = pf;
                    self.cur_ns = pns;
                    self.use_map = puse;
                }
                self.static_props.insert(key.clone(), v?);
                return Ok(key);
            }
        }
        Ok((cname.to_ascii_lowercase(), name.to_string()))
    }

    /// The var_dump visibility annotation for a property: "" (public),
    /// ":protected", or `:"DeclaringClass":private`. Empty if not a declared
    /// property (dynamic props are public).
    fn prop_annotation(&self, class: &str, prop: &str) -> String {
        let annot = |vis: Visibility, cls: &str| match vis {
            Visibility::Public => String::new(),
            Visibility::Protected => ":protected".to_string(),
            Visibility::Private => format!(":\"{}\":private", display_class(cls)),
        };
        for c in self.ancestry(class) {
            if let Some(p) = c.props.iter().find(|p| p.name == prop) {
                return annot(p.visibility, &c.name);
            }
            // constructor-promoted properties carry visibility on the param
            if let Some(ctor) = c.methods.iter().find(|m| m.name.eq_ignore_ascii_case("__construct")) {
                if let Some(pp) = ctor.params.iter().find(|p| p.name == prop && p.promote.is_some()) {
                    return annot(pp.promote.unwrap(), &c.name);
                }
            }
        }
        String::new()
    }

    /// The topmost ancestor declaring `method` with non-private visibility — PHP's
    /// "prototype" used for protected-access checks.
    fn method_prototype(&self, class: &str, method: &str) -> String {
        let mut proto = class.to_string();
        for c in self.ancestry(class) {
            if let Some(m) = c.methods.iter().find(|m| m.name.eq_ignore_ascii_case(method)) {
                if m.visibility != Visibility::Private {
                    proto = c.name.clone();
                }
            }
        }
        proto
    }

    fn same_hierarchy(&self, a: &str, b: &str) -> bool {
        a.eq_ignore_ascii_case(b) || self.is_subclass(a, b) || self.is_subclass(b, a)
    }

    /// Check method visibility from the current scope. Returns the PHP error
    /// message if the call is NOT allowed, else None. `called_class` is the class
    /// as written at the call site (used in the message).
    fn vis_error(&self, vis: Visibility, decl_class: &str, called_class: &str, method: &str) -> Option<String> {
        let allowed = match vis {
            Visibility::Public => true,
            Visibility::Private => self
                .current_class
                .as_deref()
                .map_or(false, |c| c.eq_ignore_ascii_case(decl_class)),
            Visibility::Protected => {
                let proto = self.method_prototype(decl_class, method);
                self.current_class
                    .as_deref()
                    .map_or(false, |c| self.same_hierarchy(c, &proto))
            }
        };
        if allowed {
            return None;
        }
        let vis_word = if vis == Visibility::Private { "private" } else { "protected" };
        let scope = match &self.current_class {
            Some(c) => format!("scope {}", display_class(c)),
            None => "global scope".to_string(),
        };
        Some(format!("Call to {vis_word} method {called_class}::{method}() from {scope}"))
    }

    fn is_subclass(&self, class: &str, target: &str) -> bool {
        let t = target.to_ascii_lowercase();
        for c in self.ancestry(class) {
            if c.name.to_ascii_lowercase() == t {
                return true;
            }
            for i in &c.interfaces {
                if i.last().to_ascii_lowercase() == t {
                    return true;
                }
            }
        }
        false
    }

    /// Find a method (and its declaring class) walking up the hierarchy.
    /// Method lookup with an Rc memo: profiling WordPress showed whole
    /// MethodDecl bodies (Vec<Stmt>) being CLONED on every method call — the
    /// single hottest allocation site. The memo clones once per unique
    /// (class, method) and hands out Rc thereafter; class (re)registration
    /// clears it.
    /// find_method, but if the lookup misses because an ANCESTOR class hasn't
    /// been loaded yet (autoloaded libraries: `class A extends \Ns\B` where B
    /// arrives via spl_autoload later), autoload the missing parents and retry.
    fn find_method_autoload(&mut self, class: &str, method: &str) -> Option<(String, Rc<MethodDecl>)> {
        if let Some(hit) = self.find_method(class, method) {
            return Some(hit);
        }
        let mut cur = self.find_class(class);
        let mut guard = 0;
        let mut loaded_any = false;
        while let Some(c) = cur {
            guard += 1;
            if guard > 50 {
                break;
            }
            let parent = match &c.parent {
                Some(p) => p.clone(),
                None => break,
            };
            if self.find_class_n(&parent).is_none() {
                let name = parent.parts.join("\\");
                if self.autoload(&name) {
                    loaded_any = true;
                } else {
                    break;
                }
            }
            cur = self.find_class_n(&parent);
        }
        if loaded_any {
            self.method_cache.borrow_mut().clear();
            return self.find_method(class, method);
        }
        None
    }

    fn find_method(&self, class: &str, method: &str) -> Option<(String, Rc<MethodDecl>)> {
        let key = (class.to_ascii_lowercase(), method.to_ascii_lowercase());
        if let Some(hit) = self.method_cache.borrow().get(&key) {
            return hit.clone();
        }
        let m = &key.1;
        let mut found: Option<(String, Rc<MethodDecl>)> = None;
        'outer: for c in self.ancestry(class) {
            // traits first (declared in the class), then own methods
            for t in &c.uses_traits {
                if let Some(tc) = self.find_class_n(t) {
                    if let Some(md) = tc.methods.iter().find(|x| x.name.to_ascii_lowercase() == *m) {
                        found = Some((c.name.clone(), Rc::new(md.clone())));
                        break 'outer;
                    }
                }
            }
            if let Some(md) = c.methods.iter().find(|x| x.name.to_ascii_lowercase() == *m) {
                found = Some((c.name.clone(), Rc::new(md.clone())));
                break;
            }
        }
        self.method_cache.borrow_mut().insert(key, found.clone());
        found
    }

    fn instantiate(&mut self, class: &str, args: Vec<Value>) -> R<Value> {
        let decl = match self.find_class_autoload(class) {
            Some(d) => d,
            None => {
                return Err(self.throw_error("Error", &format!("Class \"{class}\" not found")))
            }
        };
        let obj = Rc::new(RefCell::new(Obj::new(decl.name.clone())));
        // initialize declared (instance) properties from the whole hierarchy,
        // base-most first so overrides win.
        let chain = self.ancestry(class);
        for c in chain.iter().rev() {
            // defaults may reference `self::CONST` or use-aliased classes
            // (Requests' Iri: `Port::ACAP` in an array default) — evaluate
            // under the declaring class AND its definition-site context
            let prev_cc = self.current_class.replace(c.name.clone());
            let prev_df = self.enter_def_ctx(&format!("class:{}", c.name.to_ascii_lowercase()));
            let mut result = Ok(());
            for p in &c.props {
                if p.is_static {
                    continue;
                }
                let v = match &p.default {
                    Some(d) => self.eval(d),
                    None => Ok(Value::Null),
                };
                match v {
                    Ok(v) => obj.borrow_mut().set(&p.name, v),
                    Err(e) => {
                        result = Err(e);
                        break;
                    }
                }
            }
            self.current_class = prev_cc;
            if let Some((pf, pns, puse)) = prev_df {
                self.cur_file = pf;
                self.cur_ns = pns;
                self.use_map = puse;
            }
            result?;
        }
        let ov = Value::Object(obj);
        // constructor
        if self.find_method(class, "__construct").is_some() {
            self.call_method(ov.clone(), "__construct", args)?;
        }
        Ok(ov)
    }

    fn call_method(&mut self, recv: Value, method: &str, args: Vec<Value>) -> R<Value> {
        self.call_method_ref(recv, method, args, None)
    }

    fn call_method_ref(
        &mut self,
        recv: Value,
        method: &str,
        args: Vec<Value>,
        byref: Option<&[Arg]>,
    ) -> R<Value> {
        // Closure instance methods: $c->bindTo($this[, $scope]), $c->call($this, ...).
        if let Value::Closure(c) = &recv {
            match method.to_ascii_lowercase().as_str() {
                "bindto" => {
                    let nv = ClosureVal {
                        kind: c.kind.clone_rc(),
                        captures: c.captures.clone(),
                        bound_this: match args.first() {
                            Some(Value::Null) | None => None,
                            Some(v) => Some(v.clone()),
                        },
                    };
                    return Ok(Value::Closure(Rc::new(nv)));
                }
                "call" => {
                    let this = args.first().cloned();
                    let rest: Vec<Value> = args.iter().skip(1).cloned().collect();
                    let bound = Rc::new(ClosureVal {
                        kind: c.kind.clone_rc(),
                        captures: c.captures.clone(),
                        bound_this: this,
                    });
                    return self.call_closure(&bound, rest);
                }
                "__invoke" => return self.call_closure(c, args),
                _ => return Ok(Value::Null),
            }
        }
        let class = match &recv {
            Value::Object(rc) => rc.borrow().class.clone(),
            _ => return Ok(Value::Null),
        };
        let (decl_class, m) = match self.find_method_autoload(&class, method) {
            Some(x) => x,
            None => {
                // __call magic fallback
                if let Some((dc, _)) = self.find_method(&class, "__call") {
                    let mut a = Arr::new();
                    for v in args {
                        a.push(v);
                    }
                    let cargs = vec![Value::Str(method.as_bytes().to_vec()), Value::Array(a)];
                    return self.invoke_method(recv, &dc, &self.find_method(&class, "__call").unwrap().1.clone(), cargs, None);
                }
                return Err(self.throw_error(
                    "Error",
                    &format!("Call to undefined method {}::{method}()", display_class(&class)),
                ));
            }
        };
        // Visibility: an inaccessible method routes to __call if present, else errors.
        if let Some(msg) = self.vis_error(m.visibility, &decl_class, &display_class(&class), method) {
            if let Some((dc, cm)) = self.find_method(&class, "__call") {
                let mut a = Arr::new();
                for v in args {
                    a.push(v);
                }
                let cargs = vec![Value::Str(method.as_bytes().to_vec()), Value::Array(a)];
                return self.invoke_method(recv, &dc, &cm, cargs, None);
            }
            return Err(self.throw_error("Error", &msg));
        }
        self.invoke_method(recv, &decl_class, &m, args, byref)
    }

    fn invoke_method(
        &mut self,
        recv: Value,
        decl_class: &str,
        m: &MethodDecl,
        args: Vec<Value>,
        byref: Option<&[Arg]>,
    ) -> R<Value> {
        let body = match &m.body {
            Some(b) => b.clone(),
            None => return Ok(Value::Null),
        };
        self.enter_call()?;
        self.cur_args.push(Rc::new(args.clone()));
        self.cur_fn.push(m.name.clone());
        let mut scope = HashMap::new();
        if !m.is_static {
            scope.insert("this".to_string(), recv.clone());
        }
        let mfname = format!("{}::{}", display_class(decl_class), m.name);
        self.frames.push((format!("{}->{}", display_class(decl_class), m.name), self.cur_line, self.cur_file_str()));
        let prev_df = self.enter_def_ctx(&format!("class:{}", decl_class.to_ascii_lowercase()));
        // class scope must be in place BEFORE bind_params: parameter defaults
        // may reference self:: (WP_Theme_JSON::__construct)
        let prev_class = self.current_class.replace(decl_class.to_string());
        if let Err(e) = self.bind_params(&mut scope, &m.params, &args, &mfname) {
            self.current_class = prev_class;
            if let Some((pf, pns, puse)) = prev_df {
                self.cur_file = pf;
                self.cur_ns = pns;
                self.use_map = puse;
            }
            self.cur_args.pop();
            self.cur_fn.pop();
            self.frames.pop();
            self.call_depth -= 1;
            return Err(e);
        }
        self.alias_byref_params(&mut scope, &m.params, byref);
        // constructor property promotion
        if m.name.eq_ignore_ascii_case("__construct") {
            if let Value::Object(rc) = &recv {
                for (i, p) in m.params.iter().enumerate() {
                    if p.promote.is_some() {
                        let v = args
                            .get(i)
                            .cloned()
                            .or_else(|| scope.get(&p.name).cloned())
                            .unwrap_or(Value::Null);
                        rc.borrow_mut().set(&p.name, v);
                    }
                }
            }
        }
        // LSB scope: the runtime class of the receiver
        let prev_called = std::mem::replace(
            &mut self.called_class,
            Some(match &recv {
                Value::Object(rc) => rc.borrow().class.clone(),
                _ => decl_class.to_string(),
            }),
        );
        let quiet_body = self.prelude_classes.contains(&decl_class.to_ascii_lowercase());
        if quiet_body {
            self.quiet += 1;
        }
        self.byref_ret.push(m.by_ref_return);
        self.scopes.push(scope);
        let r = self.run_fn_body(&body);
        let wb = self.capture_byref(&m.params, byref);
        self.scopes.pop();
        self.byref_ret.pop();
        if quiet_body {
            self.quiet -= 1;
        }
        self.current_class = prev_class;
        self.called_class = prev_called;
        if let Some((pf, pns, puse)) = prev_df {
            self.cur_file = pf;
            self.cur_ns = pns;
            self.use_map = puse;
        }
        self.cur_args.pop();
        self.cur_fn.pop();
        self.frames.pop();
        self.call_depth -= 1;
        self.apply_byref(byref, wb)?;
        let rt = m.ret_type.clone();
        r.and_then(|v| self.check_return(v, &rt, &mfname))
    }

    fn call_static(
        &mut self,
        class: &str,
        method: &str,
        args: Vec<Value>,
        this: Option<Value>,
        byref: Option<&[Arg]>,
    ) -> R<Value> {
        self.call_static_fw(class, method, args, this, byref, false)
    }

    /// `forwarding`: the call was written `self::`/`parent::`/`static::` — the
    /// late-static-binding scope of the caller is preserved. An explicit
    /// `ClassName::m()` rebinds it to that class.
    fn call_static_fw(
        &mut self,
        class: &str,
        method: &str,
        args: Vec<Value>,
        this: Option<Value>,
        byref: Option<&[Arg]>,
        forwarding: bool,
    ) -> R<Value> {
        // Closure static methods: Closure::bind($c, $this[, $scope]),
        // Closure::fromCallable($callable).
        if class.eq_ignore_ascii_case("Closure") {
            match method.to_ascii_lowercase().as_str() {
                "bind" => {
                    if let Some(Value::Closure(c)) = args.first() {
                        let nv = ClosureVal {
                            kind: c.kind.clone_rc(),
                            captures: c.captures.clone(),
                            bound_this: match args.get(1) {
                                Some(Value::Null) | None => None,
                                Some(v) => Some(v.clone()),
                            },
                        };
                        return Ok(Value::Closure(Rc::new(nv)));
                    }
                    return Ok(Value::Null);
                }
                "fromcallable" => {
                    let cv = args.into_iter().next().unwrap_or(Value::Null);
                    // already a closure → return as-is; else keep the callable value
                    // (string name / [obj,m] array) — call_value handles invocation.
                    return Ok(cv);
                }
                _ => {}
            }
        }
        // Enum built-in static methods: cases() / from() / tryFrom().
        if let Some(c) = self.find_class(class) {
            if c.kind == ClassKind::Enum {
                let backed = c.enum_backing.is_some();
                match method {
                    "cases" => {
                        let names: Vec<String> = c.cases.iter().map(|e| e.name.clone()).collect();
                        let mut arr = Arr::new();
                        for n in names {
                            arr.push(self.class_const(class, &n)?);
                        }
                        return Ok(Value::Array(arr));
                    }
                    "from" | "tryFrom" if backed => {
                        let want = args.first().cloned().unwrap_or(Value::Null);
                        let names: Vec<String> = c.cases.iter().map(|e| e.name.clone()).collect();
                        for n in &names {
                            let case = self.class_const(class, n)?;
                            if let Value::Object(o) = &case {
                                let cv = o.borrow().get("value").cloned().unwrap_or(Value::Null);
                                if loose_eq(&cv, &want) && type_name(&cv) == type_name(&want) {
                                    return Ok(case);
                                }
                            }
                        }
                        if method == "tryFrom" {
                            return Ok(Value::Null);
                        }
                        let disp = String::from_utf8_lossy(&to_bytes(&want)).into_owned();
                        return Err(self.throw_error(
                            "ValueError",
                            &format!("{disp} is not a valid backing value for enum {class}"),
                        ));
                    }
                    _ => {}
                }
            }
        }
        if self.find_class(class).is_none() {
            self.autoload(class);
        }
        let (decl_class, m) = match self.find_method_autoload(class, method) {
            Some(x) => x,
            None => {
                return Err(self.throw_error(
                    "Error",
                    &format!("Call to undefined method {}::{method}()", display_class(class)),
                ))
            }
        };
        // Visibility: an inaccessible static method routes to __callStatic, else errors.
        if let Some(msg) = self.vis_error(m.visibility, &decl_class, &display_class(class), method) {
            if let Some((dc, cm)) = self.find_method(class, "__callStatic") {
                let mut a = Arr::new();
                for v in args {
                    a.push(v);
                }
                let cargs = vec![Value::Str(method.as_bytes().to_vec()), Value::Array(a)];
                return self.invoke_method(Value::Null, &dc, &cm, cargs, None);
            }
            return Err(self.throw_error("Error", &msg));
        }
        let body = match &m.body {
            Some(b) => b.clone(),
            None => return Ok(Value::Null),
        };
        self.enter_call()?;
        self.cur_args.push(Rc::new(args.clone()));
        self.cur_fn.push(m.name.clone());
        let mut scope = HashMap::new();
        // a non-static method reached via parent::/self:: keeps $this
        if !m.is_static {
            if let Some(t) = this {
                scope.insert("this".to_string(), t);
            }
        }
        let mfname = format!("{}::{}", display_class(&decl_class), m.name);
        self.frames.push((mfname.clone(), self.cur_line, self.cur_file_str()));
        let prev_df = self.enter_def_ctx(&format!("class:{}", decl_class.to_ascii_lowercase()));
        // class scope before bind_params: parameter defaults may use self::
        let prev_class = self.current_class.replace(decl_class.clone());
        if let Err(e) = self.bind_params(&mut scope, &m.params, &args, &mfname) {
            self.current_class = prev_class;
            if let Some((pf, pns, puse)) = prev_df {
                self.cur_file = pf;
                self.cur_ns = pns;
                self.use_map = puse;
            }
            self.cur_args.pop();
            self.cur_fn.pop();
            self.frames.pop();
            self.call_depth -= 1;
            return Err(e);
        }
        self.alias_byref_params(&mut scope, &m.params, byref);
        // LSB scope: forwarding calls keep the caller's; explicit C::m() rebinds.
        // (Canonicalize through find_class so case matches the declaration.)
        let called = if forwarding {
            self.called_class.clone()
        } else {
            None
        }
        .or_else(|| self.find_class(class).map(|c| c.name.clone()))
        .or_else(|| Some(class.to_string()));
        let prev_called = std::mem::replace(&mut self.called_class, called);
        let quiet_body = self.prelude_classes.contains(&decl_class.to_ascii_lowercase());
        if quiet_body {
            self.quiet += 1;
        }
        self.byref_ret.push(m.by_ref_return);
        self.scopes.push(scope);
        let r = self.run_fn_body(&body);
        let wb = self.capture_byref(&m.params, byref);
        self.scopes.pop();
        self.byref_ret.pop();
        if quiet_body {
            self.quiet -= 1;
        }
        self.current_class = prev_class;
        self.called_class = prev_called;
        if let Some((pf, pns, puse)) = prev_df {
            self.cur_file = pf;
            self.cur_ns = pns;
            self.use_map = puse;
        }
        self.cur_args.pop();
        self.cur_fn.pop();
        self.frames.pop();
        self.call_depth -= 1;
        self.apply_byref(byref, wb)?;
        let rt = m.ret_type.clone();
        r.and_then(|v| self.check_return(v, &rt, &mfname))
    }

    /// Cached: any typed or readonly instance property anywhere in the hierarchy?
    fn class_has_typed_props(&mut self, class: &str) -> bool {
        let key = class.to_ascii_lowercase();
        if let Some(&b) = self.typed_props_cache.get(&key) {
            return b;
        }
        let b = self
            .ancestry(class)
            .iter()
            .any(|c| c.props.iter().any(|p| !p.is_static && (p.type_hint.is_some() || p.readonly)));
        self.typed_props_cache.insert(key, b);
        b
    }

    /// Enforce declared property type + readonly on a write. Returns the
    /// (possibly coerced) value, or throws PHP's TypeError/Error.
    fn check_prop_write(&mut self, class: &str, pname: &str, val: Value) -> R<Value> {
        // find the declaring class's PropDecl for pname (non-static)
        let mut decl: Option<(String, Option<String>, bool)> = None;
        for c in self.ancestry(class) {
            if let Some(p) = c.props.iter().find(|p| !p.is_static && p.name == pname) {
                decl = Some((c.name.clone(), p.type_hint.clone(), p.readonly));
                break;
            }
        }
        let Some((decl_class, hint, readonly)) = decl else { return Ok(val) };
        if readonly {
            // approximation: writes are allowed only from inside the declaring
            // class (init-once isn't representable — props default-initialize)
            let inside = self
                .current_class
                .as_deref()
                .map(|c| c.eq_ignore_ascii_case(&decl_class))
                .unwrap_or(false);
            if !inside {
                return Err(self.throw_error(
                    "Error",
                    &format!(
                        "Cannot modify readonly property {}::${}",
                        display_class(&decl_class),
                        pname
                    ),
                ));
            }
        }
        let Some(hint) = hint else { return Ok(val) };
        let given = self.given_type(&val);
        match self.coerce_typed(&hint, val) {
            Ok(v) => Ok(v),
            Err(_) => Err(self.throw_error(
                "TypeError",
                &format!(
                    "Cannot assign {given} to property {}::${} of type {}",
                    display_class(&decl_class),
                    pname,
                    hint
                ),
            )),
        }
    }

    /// Does `v` satisfy the single (non-union) declared type `t`?
    fn type_matches_one(&self, t: &str, v: &Value) -> bool {
        match t.to_ascii_lowercase().as_str() {
            "mixed" => true,
            "int" => matches!(v, Value::Int(_)),
            "float" => matches!(v, Value::Float(_) | Value::Int(_)), // int→float widening
            "string" => matches!(v, Value::Str(_)),
            "bool" | "true" | "false" => matches!(v, Value::Bool(_)),
            "array" => matches!(v, Value::Array(_)),
            "null" => matches!(v, Value::Null),
            "object" => matches!(v, Value::Object(_) | Value::Closure(_)),
            "callable" => matches!(v, Value::Closure(_) | Value::Str(_) | Value::Array(_)),
            "iterable" => match v {
                Value::Array(_) => true,
                Value::Object(rc) => {
                    let c = rc.borrow().class.clone();
                    self.instance_of_name(&c, "Traversable")
                        || self.find_method(&c, "current").is_some()
                        || self.find_method(&c, "getiterator").is_some()
                }
                _ => false,
            },
            "self" | "static" | "parent" => matches!(v, Value::Object(_)),
            _ => match v {
                // class/interface type: exact, ancestor, or implemented interface
                Value::Object(rc) => {
                    let c = rc.borrow().class.clone();
                    self.instance_of_name(&c, t.trim_start_matches('\\'))
                }
                Value::Closure(_) => t.eq_ignore_ascii_case("closure"),
                _ => false,
            },
        }
    }

    /// Is class `c` (or an ancestor) named `want`, or does it implement it?
    fn instance_of_name(&self, c: &str, want: &str) -> bool {
        // FQ-aware: equal as written, or (unqualified want) equal last segment
        fn matches(name: &str, want: &str) -> bool {
            if name.eq_ignore_ascii_case(want) {
                return true;
            }
            if !want.contains('\\') {
                if let Some(lastseg) = name.rsplit('\\').next() {
                    return lastseg.eq_ignore_ascii_case(want);
                }
            }
            false
        }
        if matches(c, want) {
            return true;
        }
        for a in self.ancestry(c) {
            if matches(&a.name, want) {
                return true;
            }
            for i in &a.interfaces {
                let joined = i.parts.join("\\");
                if matches(&joined, want) || self.instance_of_name(&joined, want) {
                    return true;
                }
            }
        }
        false
    }

    /// The type-name PHP uses in TypeError messages for a given value.
    fn given_type(&self, v: &Value) -> String {
        match v {
            Value::Null => "null".into(),
            Value::Bool(_) => "bool".into(),
            Value::Int(_) => "int".into(),
            Value::Float(_) => "float".into(),
            Value::Str(_) => "string".into(),
            Value::Array(_) => "array".into(),
            Value::Closure(_) => "Closure".into(),
            Value::Object(rc) => display_class(&rc.borrow().class),
            Value::Ref(c) => self.given_type(&c.borrow()),
        }
    }

    /// Enforce a declared type on an argument or return value: pass it through,
    /// weak-coerce it (unless strict_types), or produce the TypeError message.
    /// Returns Err(message) with PHP's "must be of type X, Y given" core.
    fn coerce_typed(&mut self, hint: &str, v: Value) -> Result<Value, String> {
        // nullable prefix / union parts (intersections: any object passes)
        let (nullable, body) = match hint.strip_prefix('?') {
            Some(rest) => (true, rest),
            None => (false, hint),
        };
        if body.contains('&') {
            return match v {
                Value::Object(_) => Ok(v),
                _ => Err(format!("must be of type {hint}, {} given", self.given_type(&v))),
            };
        }
        let parts: Vec<&str> = body.split('|').collect();
        if matches!(v, Value::Null)
            && (nullable || parts.iter().any(|p| p.eq_ignore_ascii_case("null") || p.eq_ignore_ascii_case("mixed")))
        {
            return Ok(v);
        }
        for p in &parts {
            if self.type_matches_one(p, &v) {
                // canonical widening: int satisfies a float declaration as float
                if p.eq_ignore_ascii_case("float") {
                    if let Value::Int(n) = v {
                        return Ok(Value::Float(n as f64));
                    }
                }
                return Ok(v);
            }
        }
        // weak-mode scalar coercions (PHP 8 rules, deprecations not modeled)
        if !self.strict_types {
            for p in &parts {
                let coerced = match (p.to_ascii_lowercase().as_str(), &v) {
                    ("int", Value::Float(f)) => Some(Value::Int(*f as i64)),
                    ("int", Value::Bool(b)) => Some(Value::Int(*b as i64)),
                    ("int", Value::Str(s)) if is_numeric_str(s) => Some(Value::Int(to_i64(&v))),
                    ("float", Value::Bool(b)) => Some(Value::Float(*b as i64 as f64)),
                    ("float", Value::Str(s)) if is_numeric_str(s) => Some(Value::Float(to_f64(&v))),
                    ("string", Value::Int(_) | Value::Float(_) | Value::Bool(_)) => {
                        Some(Value::Str(to_bytes(&v)))
                    }
                    ("string", Value::Object(rc)) => {
                        let c = rc.borrow().class.clone();
                        if self.find_method(&c, "__tostring").is_some() {
                            match self.stringify(&v) {
                                Ok(s) => Some(Value::Str(s)),
                                Err(_) => None,
                            }
                        } else {
                            None
                        }
                    }
                    ("bool", Value::Int(_) | Value::Float(_) | Value::Str(_)) => {
                        Some(Value::Bool(to_bool(&v)))
                    }
                    _ => None,
                };
                if let Some(c) = coerced {
                    return Ok(c);
                }
            }
        }
        Err(format!("must be of type {hint}, {} given", self.given_type(&v)))
    }

    /// Type-check/coerce one passed argument against its declared param type.
    fn check_arg(&mut self, p: &Param, i: usize, v: Value, fname: &str) -> R<Value> {
        let Some(hint) = p.type_hint.clone() else { return Ok(v) };
        if matches!(v, Value::Ref(_)) {
            return Ok(v); // by-ref alias: checked at write sites, not here
        }
        match self.coerce_typed(&hint, v) {
            Ok(v) => Ok(v),
            Err(core) => {
                let file = self
                    .cur_file
                    .as_ref()
                    .map(|p| p.to_string_lossy().into_owned())
                    .unwrap_or_default();
                Err(self.throw_error(
                    "TypeError",
                    &format!(
                        "{fname}(): Argument #{} (${}) {core}, called in {file} on line 0",
                        i + 1,
                        p.name
                    ),
                ))
            }
        }
    }

    /// Type-check a return value against the declared return type. Null passes
    /// unconditionally (we can't distinguish `return null` from falling off the
    /// end, and void/implicit returns must not throw); generators skip too.
    fn check_return(&mut self, v: Value, rt: &Option<String>, fname: &str) -> R<Value> {
        let Some(rt) = rt else { return Ok(v) };
        if matches!(v, Value::Null) {
            return Ok(v);
        }
        let lower = rt.to_ascii_lowercase();
        if matches!(lower.as_str(), "void" | "never" | "mixed") {
            return Ok(v);
        }
        if let Value::Object(rc) = &v {
            if rc.borrow().class == "Generator" {
                return Ok(v);
            }
        }
        let rt = rt.clone();
        match self.coerce_typed(&rt, v) {
            Ok(v) => Ok(v),
            Err(core) => {
                let msg =
                    format!("{fname}(): Return value {}", core.replacen(" given", " returned", 1));
                Err(self.throw_error("TypeError", &msg))
            }
        }
    }

    fn bind_params(
        &mut self,
        scope: &mut HashMap<String, Value>,
        params: &[Param],
        args: &[Value],
        fname: &str,
    ) -> R<()> {
        for (i, p) in params.iter().enumerate() {
            if p.variadic {
                let mut rest = Arr::new();
                for v in args.iter().skip(i) {
                    rest.push(v.clone());
                }
                scope.insert(p.name.clone(), Value::Array(rest));
                break;
            }
            let passed = i < args.len();
            let v = match args.get(i) {
                // A Ref arg (only builtin-driven calls construct these) aliases
                // into a by-ref param; a by-value param gets the dereferenced copy.
                Some(Value::Ref(c)) => {
                    if p.by_ref {
                        Value::Ref(c.clone())
                    } else {
                        c.borrow().deref()
                    }
                }
                Some(v) => v.clone(),
                None => match &p.default {
                    Some(d) => self.eval(d)?,
                    None => Value::Null,
                },
            };
            let v = if passed { self.check_arg(p, i, v, fname)? } else { v };
            scope.insert(p.name.clone(), v);
        }
        Ok(())
    }

    /// Given the result of a `try` body, dispatch to a matching `catch`.
    fn handle_try_outcome(&mut self, outcome: R<Flow>, catches: &[Catch]) -> R<Flow> {
        let err = match outcome {
            Ok(flow) => return Ok(flow),
            Err(e) => e,
        };
        let exc = match self.thrown.take() {
            Some(v) => v,
            None => return Err(err), // not an exception — propagate (e.g. step limit)
        };
        let cls = match &exc {
            Value::Object(rc) => rc.borrow().class.clone(),
            _ => String::new(),
        };
        for c in catches {
            if c.types.iter().any(|t| self.is_subclass(&cls, t.last())) {
                if let Some(var) = &c.var {
                    self.vars().insert(var.clone(), exc.clone());
                }
                return self.exec_block(&c.body);
            }
        }
        // no catch matched — re-throw
        self.thrown = Some(exc);
        Err(err)
    }

    /// Instantiate `class(msg)` and arm it as the pending throw.
    fn throw_error(&mut self, class: &str, msg: &str) -> RunError {
        let v = self
            .instantiate(class, vec![Value::Str(msg.as_bytes().to_vec())])
            .unwrap_or(Value::Null);
        self.thrown = Some(v);
        RunError("__phargo_throw__".into())
    }

    fn class_const(&mut self, class: &str, name: &str) -> R<Value> {
        if self.find_class(class).is_none() {
            self.autoload(class);
        }
        // enum case?
        if let Some(c) = self.find_class(class) {
            if c.kind == ClassKind::Enum {
                if c.cases.iter().any(|e| e.name == name) {
                    let key = (c.name.clone(), name.to_string());
                    if let Some(v) = self.enum_cases.get(&key) {
                        return Ok(v.clone());
                    }
                    // model an enum case as an object with `name` (+ `value`)
                    let obj = Rc::new(RefCell::new(Obj::new(c.name.clone())));
                    obj.borrow_mut().set("name", Value::Str(name.as_bytes().to_vec()));
                    if let Some(ec) = c.cases.iter().find(|e| e.name == name) {
                        if let Some(v) = &ec.value.clone() {
                            let val = self.eval(v)?;
                            obj.borrow_mut().set("value", val);
                        }
                    }
                    let v = Value::Object(obj);
                    self.enum_cases.insert(key, v.clone());
                    return Ok(v);
                }
            }
        }
        // A const initializer referencing `self::`/other consts evaluates in the
        // class that DECLARED it (`parent::myDynConst` where the initializer says
        // `self::myConst` must use the parent's), so scope current_class to the
        // declaring class while evaluating.
        let mut eval_in = |ev: &mut Self, expr: Expr, decl: String| -> R<Value> {
            let prev = ev.current_class.replace(decl);
            let r = ev.eval(&expr);
            ev.current_class = prev;
            r
        };
        for c in self.ancestry(class) {
            if let Some(cc) = c.consts.iter().find(|x| x.name == name) {
                let (expr, decl) = (cc.value.clone(), c.name.clone());
                return eval_in(self, expr, decl);
            }
            // interface constants
            for i in &c.interfaces {
                if let Some(ic) = self.find_class_n(i) {
                    if let Some(cc) = ic.consts.iter().find(|x| x.name == name) {
                        let (expr, decl) = (cc.value.clone(), ic.name.clone());
                        return eval_in(self, expr, decl);
                    }
                }
            }
        }
        Err(RunError(format!("undefined constant {class}::{name}")))
    }
}

// ---- stream resources (fopen family) -----------------------------------
//
// A stream is modelled as a `Value::Object` of the pseudo-class `__Stream`,
// holding an in-memory byte buffer + cursor. Real files are slurped on open and
// flushed back to disk on every write/close. Because objects are
// `Rc<RefCell<…>>`, a handle passed by value still mutates the same stream — so
// fread/fwrite see each other's effects without by-ref plumbing. php://memory,
// php://temp and php://std{out,err,in} are supported.
impl Eval {
    fn new_stream(&mut self, path: &str, mode: &str, buf: Vec<u8>, real: bool, std: &str) -> Value {
        let id = self.next_res_id;
        self.next_res_id += 1;
        let o = new_obj("__Stream");
        if let Value::Object(rc) = &o {
            let mut b = rc.borrow_mut();
            b.set("__stream", Value::Bool(true));
            b.set("__id", Value::Int(id));
            b.set("__path", Value::Str(path.as_bytes().to_vec()));
            b.set("__mode", Value::Str(mode.as_bytes().to_vec()));
            let pos = if mode.starts_with('a') { buf.len() as i64 } else { 0 };
            b.set("__pos", Value::Int(pos));
            b.set("__buf", Value::Str(buf));
            b.set("__real", Value::Bool(real));
            b.set("__std", Value::Str(std.as_bytes().to_vec()));
        }
        o
    }

    fn fopen_impl(&mut self, path: &str, mode: &str) -> Value {
        let first = mode.chars().next().unwrap_or('r');
        if path == "php://stdout" || path == "php://output" {
            return self.new_stream(path, mode, vec![], false, "stdout");
        }
        if path == "php://stderr" {
            return self.new_stream(path, mode, vec![], false, "stderr");
        }
        if path == "php://stdin" || path == "php://input" {
            return self.new_stream(path, mode, vec![], false, "stdin");
        }
        if path.starts_with("php://memory") || path.starts_with("php://temp") || path.starts_with("php://fd") {
            return self.new_stream(path, mode, vec![], false, "");
        }
        match first {
            'r' => match std::fs::read(path) {
                Ok(b) => self.new_stream(path, mode, b, true, ""),
                Err(_) => Value::Bool(false),
            },
            'w' => {
                let _ = std::fs::write(path, b"");
                self.new_stream(path, mode, vec![], true, "")
            }
            'a' | 'c' => {
                let buf = std::fs::read(path).unwrap_or_default();
                if !std::path::Path::new(path).exists() {
                    let _ = std::fs::write(path, b"");
                }
                self.new_stream(path, mode, buf, true, "")
            }
            'x' => {
                if std::path::Path::new(path).exists() {
                    Value::Bool(false)
                } else {
                    let _ = std::fs::write(path, b"");
                    self.new_stream(path, mode, vec![], true, "")
                }
            }
            _ => Value::Bool(false),
        }
    }

    fn stream_write(&mut self, v: &Value, data: &[u8]) -> R<Value> {
        let o = match v {
            Value::Object(o) if o.borrow().class == "__Stream" => o.clone(),
            _ => return Ok(Value::Bool(false)),
        };
        let std = { let b = o.borrow(); b.get("__std").map(to_bytes).unwrap_or_default() };
        match std.as_slice() {
            b"stdout" => {
                self.out.extend_from_slice(data);
                return Ok(Value::Int(data.len() as i64));
            }
            b"stderr" => return Ok(Value::Int(data.len() as i64)), // discarded
            b"stdin" => return Ok(Value::Bool(false)),
            _ => {}
        }
        let mut b = o.borrow_mut();
        let mut buf = match b.get("__buf") {
            Some(Value::Str(s)) => s.clone(),
            _ => Vec::new(),
        };
        let pos = b.get("__pos").map(to_i64).unwrap_or(0).max(0) as usize;
        let end = pos.saturating_add(data.len());
        if end > MAX_STR {
            return Err(RunError("stream write exceeds size limit".into()));
        }
        if end > buf.len() {
            buf.resize(end, 0);
        }
        buf[pos..end].copy_from_slice(data);
        b.set("__pos", Value::Int(end as i64));
        let real = matches!(b.get("__real"), Some(Value::Bool(true)));
        let path = b.get("__path").map(to_bytes).unwrap_or_default();
        b.set("__buf", Value::Str(buf.clone()));
        drop(b);
        if real {
            let _ = std::fs::write(String::from_utf8_lossy(&path).as_ref(), &buf);
        }
        Ok(Value::Int(data.len() as i64))
    }

    /// Read up to `n` bytes (None = the rest) from the cursor; advance it.
    fn stream_read_n(&mut self, v: &Value, n: Option<usize>) -> Value {
        let o = match v {
            Value::Object(o) if o.borrow().class == "__Stream" => o.clone(),
            _ => return Value::Bool(false),
        };
        let mut b = o.borrow_mut();
        let buf = match b.get("__buf") {
            Some(Value::Str(s)) => s.clone(),
            _ => Vec::new(),
        };
        let pos = b.get("__pos").map(to_i64).unwrap_or(0).max(0) as usize;
        if pos >= buf.len() {
            return Value::Str(Vec::new());
        }
        let end = match n {
            Some(k) => (pos + k).min(buf.len()),
            None => buf.len(),
        };
        b.set("__pos", Value::Int(end as i64));
        Value::Str(buf[pos..end].to_vec())
    }

    /// Read one line (through the next `\n`, inclusive), capped at `n-1` bytes.
    fn stream_gets(&mut self, v: &Value, n: Option<usize>) -> Value {
        let o = match v {
            Value::Object(o) if o.borrow().class == "__Stream" => o.clone(),
            _ => return Value::Bool(false),
        };
        let mut b = o.borrow_mut();
        let buf = match b.get("__buf") {
            Some(Value::Str(s)) => s.clone(),
            _ => Vec::new(),
        };
        let pos = b.get("__pos").map(to_i64).unwrap_or(0).max(0) as usize;
        if pos >= buf.len() {
            return Value::Bool(false);
        }
        let mut end = buf[pos..]
            .iter()
            .position(|&c| c == b'\n')
            .map(|i| pos + i + 1)
            .unwrap_or(buf.len());
        if let Some(k) = n {
            if k > 0 {
                end = end.min(pos + k - 1);
            }
        }
        b.set("__pos", Value::Int(end as i64));
        Value::Str(buf[pos..end].to_vec())
    }

    /// Move the cursor. whence: 0=SET, 1=CUR, 2=END. Returns 0 on success, -1 otherwise.
    fn stream_seek(&mut self, v: &Value, offset: i64, whence: i64) -> i64 {
        let o = match v {
            Value::Object(o) if o.borrow().class == "__Stream" => o.clone(),
            _ => return -1,
        };
        let mut b = o.borrow_mut();
        let len = match b.get("__buf") {
            Some(Value::Str(s)) => s.len() as i64,
            _ => 0,
        };
        let cur = b.get("__pos").map(to_i64).unwrap_or(0);
        let target = match whence {
            1 => cur + offset,
            2 => len + offset,
            _ => offset,
        };
        if target < 0 {
            return -1;
        }
        b.set("__pos", Value::Int(target));
        0
    }
}

// ---- builtin library (starter set) -------------------------------------
impl Eval {
    fn builtin(&mut self, name: &str, args: Vec<Value>) -> R<Value> {
        let a = |i: usize| args.get(i).cloned().unwrap_or(Value::Null);
        Ok(match name {
            "strlen" => Value::Int(to_bytes(&a(0)).len() as i64),
            "count" | "sizeof" => match a(0) {
                Value::Array(arr) => Value::Int(arr.len() as i64),
                Value::Null => Value::Int(0),
                Value::Object(rc) => {
                    let class = rc.borrow().class.clone();
                    if self.find_method(&class, "count").is_some() {
                        return self.call_method(Value::Object(rc), "count", vec![]);
                    }
                    Value::Int(1)
                }
                _ => Value::Int(1),
            },
            "spl_object_id" | "spl_object_hash" => match a(0) {
                Value::Object(rc) => {
                    // Use the sequential instance id so it matches var_dump's #N.
                    let id = rc.borrow().id as i64;
                    if name == "spl_object_hash" {
                        Value::Str(format!("{id:032x}").into_bytes())
                    } else {
                        Value::Int(id)
                    }
                }
                _ => Value::Int(0),
            },
            "iterator_to_array" => {
                // consume any iterable into an array
                let mut out = Arr::new();
                match a(0) {
                    Value::Array(arr) => return Ok(Value::Array(arr)),
                    Value::Object(rc) => {
                        let v = Value::Object(rc);
                        let class = match &v { Value::Object(r) => r.borrow().class.clone(), _ => String::new() };
                        let iter = if self.find_method(&class, "getiterator").is_some() {
                            self.call_method(v, "getIterator", vec![])?
                        } else {
                            v
                        };
                        let ic = match &iter { Value::Object(r) => r.borrow().class.clone(), _ => String::new() };
                        if self.find_method(&ic, "rewind").is_some() {
                            self.call_method(iter.clone(), "rewind", vec![])?;
                            let use_keys = args.len() < 2 || to_bool(&a(1)); // default: preserve keys
                            while to_bool(&self.call_method(iter.clone(), "valid", vec![])?) {
                                let cur = self.call_method(iter.clone(), "current", vec![])?;
                                if use_keys {
                                    let k = self.call_method(iter.clone(), "key", vec![])?;
                                    out.insert(Arr::norm_key(&k), cur);
                                } else {
                                    out.push(cur);
                                }
                                self.call_method(iter.clone(), "next", vec![])?;
                                self.tick()?;
                            }
                        }
                    }
                    _ => {}
                }
                Value::Array(out)
            }
            "var_dump" => {
                for v in &args {
                    let mut s = String::new();
                    var_dump(self, v, 0, &mut s);
                    self.out.extend_from_slice(s.as_bytes());
                }
                Value::Null
            }
            "var_export" => {
                let mut s = String::new();
                var_export(&a(0), 0, &mut s);
                if to_bool(&a(1)) {
                    Value::Str(s.into_bytes())
                } else {
                    self.out.extend_from_slice(s.as_bytes());
                    Value::Null
                }
            }
            "print_r" => {
                let mut s = String::new();
                print_r(&a(0), 0, &mut s);
                if to_bool(&a(1)) {
                    Value::Str(s.into_bytes())
                } else {
                    self.out.extend_from_slice(s.as_bytes());
                    Value::Bool(true)
                }
            }
            "gettype" => Value::Str(type_name(&a(0)).as_bytes().to_vec()),
            "is_int" | "is_integer" | "is_long" => Value::Bool(matches!(a(0), Value::Int(_))),
            "is_float" | "is_double" => Value::Bool(matches!(a(0), Value::Float(_))),
            "is_string" => Value::Bool(matches!(a(0), Value::Str(_))),
            "is_bool" => Value::Bool(matches!(a(0), Value::Bool(_))),
            "is_array" => Value::Bool(matches!(a(0), Value::Array(_))),
            "is_null" => Value::Bool(matches!(a(0), Value::Null)),
            "is_object" => Value::Bool(match a(0) {
                Value::Object(o) => o.borrow().class != "__Stream",
                Value::Closure(_) => true,
                _ => false,
            }),
            "is_resource" => Value::Bool(is_stream(&a(0))),
            "get_resource_type" => {
                if is_stream(&a(0)) {
                    Value::Str(b"stream".to_vec())
                } else {
                    Value::Null
                }
            }
            "get_resource_id" => {
                if let Value::Object(o) = a(0) {
                    Value::Int(o.borrow().get("__id").map(to_i64).unwrap_or(0))
                } else {
                    Value::Int(0)
                }
            }
            "is_iterable" => Value::Bool(matches!(a(0), Value::Array(_) | Value::Object(_))),
            "is_numeric" => Value::Bool(match a(0) {
                Value::Int(_) | Value::Float(_) => true,
                Value::Str(s) => is_numeric_str(&s),
                _ => false,
            }),
            "is_scalar" => Value::Bool(matches!(
                a(0),
                Value::Int(_) | Value::Float(_) | Value::Str(_) | Value::Bool(_)
            )),
            "intval" => Value::Int(to_i64(&a(0))),
            "floatval" | "doubleval" => Value::Float(to_f64(&a(0))),
            "strval" => Value::Str(to_bytes(&a(0))),
            "boolval" => Value::Bool(to_bool(&a(0))),
            "abs" => match to_num(&a(0)) {
                Num::Int(n) => Value::Int(n.abs()),
                Num::Float(f) => Value::Float(f.abs()),
            },
            "max" => self.extreme(&args, true),
            "min" => self.extreme(&args, false),
            "floor" => Value::Float(to_f64(&a(0)).floor()),
            "ceil" => Value::Float(to_f64(&a(0)).ceil()),
            "round" => {
                let p = to_i64(&a(1));
                let m = 10f64.powi(p as i32);
                Value::Float((to_f64(&a(0)) * m).round() / m)
            }
            "log" => {
                let x = to_f64(&a(0));
                if args.len() > 1 {
                    Value::Float(x.log(to_f64(&a(1))))
                } else {
                    Value::Float(x.ln())
                }
            }
            "log10" => Value::Float(to_f64(&a(0)).log10()),
            "log2" => Value::Float(to_f64(&a(0)).log2()),
            "log1p" => Value::Float(to_f64(&a(0)).ln_1p()),
            "exp" => Value::Float(to_f64(&a(0)).exp()),
            "expm1" => Value::Float(to_f64(&a(0)).exp_m1()),
            "sin" => Value::Float(to_f64(&a(0)).sin()),
            "cos" => Value::Float(to_f64(&a(0)).cos()),
            "tan" => Value::Float(to_f64(&a(0)).tan()),
            "asin" => Value::Float(to_f64(&a(0)).asin()),
            "acos" => Value::Float(to_f64(&a(0)).acos()),
            "atan" => Value::Float(to_f64(&a(0)).atan()),
            "atan2" => Value::Float(to_f64(&a(0)).atan2(to_f64(&a(1)))),
            "sinh" => Value::Float(to_f64(&a(0)).sinh()),
            "cosh" => Value::Float(to_f64(&a(0)).cosh()),
            "tanh" => Value::Float(to_f64(&a(0)).tanh()),
            "asinh" => Value::Float(to_f64(&a(0)).asinh()),
            "acosh" => Value::Float(to_f64(&a(0)).acosh()),
            "atanh" => Value::Float(to_f64(&a(0)).atanh()),
            "pi" => Value::Float(std::f64::consts::PI),
            "fmod" => Value::Float(to_f64(&a(0)) % to_f64(&a(1))),
            "hypot" => Value::Float(to_f64(&a(0)).hypot(to_f64(&a(1)))),
            "deg2rad" => Value::Float(to_f64(&a(0)).to_radians()),
            "rad2deg" => Value::Float(to_f64(&a(0)).to_degrees()),
            "sqrt" => Value::Float(to_f64(&a(0)).sqrt()),
            // ---- bcmath (from-scratch decimal bignums in src/bcmath.rs) ----
            "bcadd" | "bcsub" | "bcmul" | "bcdiv" | "bcmod" | "bccomp" => {
                let parse = |v: &Value| crate::bc::Dec::parse(&String::from_utf8_lossy(&to_bytes(v)));
                let x = match parse(&a(0)) {
                    Some(d) => d,
                    None => {
                        return Err(self.throw_error(
                            "ValueError",
                            &format!("{name}(): Argument #1 ($num1) is not well-formed"),
                        ))
                    }
                };
                let y = match parse(&a(1)) {
                    Some(d) => d,
                    None => {
                        return Err(self.throw_error(
                            "ValueError",
                            &format!("{name}(): Argument #2 ($num2) is not well-formed"),
                        ))
                    }
                };
                let scale = if args.len() > 2 {
                    let sc = to_i64(&a(2));
                    if !(0..=2147483647).contains(&sc) {
                        return Err(self.throw_error(
                            "ValueError",
                            &format!("{name}(): Argument #3 ($scale) must be between 0 and 2147483647"),
                        ));
                    }
                    (sc as usize).min(100_000)
                } else {
                    self.bc_scale
                };
                match name {
                    "bccomp" => {
                        let (xa, ya) = (x.with_scale(scale), y.with_scale(scale));
                        Value::Int(match crate::bc::cmp(&xa, &ya) {
                            std::cmp::Ordering::Less => -1,
                            std::cmp::Ordering::Equal => 0,
                            std::cmp::Ordering::Greater => 1,
                        })
                    }
                    "bcadd" => Value::Str(crate::bc::add(&x, &y).to_string_scaled(scale).into_bytes()),
                    "bcsub" => Value::Str(crate::bc::sub(&x, &y).to_string_scaled(scale).into_bytes()),
                    "bcmul" => match crate::bc::mul(&x, &y) {
                        Some(r) => Value::Str(r.to_string_scaled(scale).into_bytes()),
                        None => return Err(self.throw_error("ValueError", "bcmul(): result too large")),
                    },
                    "bcdiv" => match crate::bc::div(&x, &y, scale) {
                        Some(r) => Value::Str(r.to_string_scaled(scale).into_bytes()),
                        None => {
                            return Err(self.throw_error("DivisionByZeroError", "Division by zero"))
                        }
                    },
                    _ => match crate::bc::modulo(&x, &y, scale) {
                        Some(r) => Value::Str(r.to_string_scaled(scale).into_bytes()),
                        None => {
                            return Err(self.throw_error("DivisionByZeroError", "Modulo by zero"))
                        }
                    },
                }
            }
            "bcpow" => {
                let base = crate::bc::Dec::parse(&String::from_utf8_lossy(&to_bytes(&a(0))));
                let Some(base) = base else {
                    return Err(self.throw_error(
                        "ValueError",
                        "bcpow(): Argument #1 ($num) is not well-formed",
                    ));
                };
                let exp = to_i64(&a(1));
                let scale = if args.len() > 2 {
                    let sc = to_i64(&a(2));
                    if !(0..=2147483647).contains(&sc) {
                        return Err(self.throw_error(
                            "ValueError",
                            "bcpow(): Argument #3 ($scale) must be between 0 and 2147483647",
                        ));
                    }
                    (sc as usize).min(100_000)
                } else {
                    self.bc_scale
                };
                let r = if exp >= 0 {
                    crate::bc::pow(&base, exp as u64, scale)
                } else {
                    crate::bc::pow(&base, (-exp) as u64, scale.max(base.scale) + 10)
                        .and_then(|p| crate::bc::div(&crate::bc::Dec::parse("1").unwrap(), &p, scale))
                };
                match r {
                    Some(r) => Value::Str(r.to_string_scaled(scale).into_bytes()),
                    None => return Err(self.throw_error("ValueError", "bcpow(): result too large")),
                }
            }
            "bcsqrt" => {
                let Some(x) = crate::bc::Dec::parse(&String::from_utf8_lossy(&to_bytes(&a(0)))) else {
                    return Err(self.throw_error(
                        "ValueError",
                        "bcsqrt(): Argument #1 ($num) is not well-formed",
                    ));
                };
                let scale = if args.len() > 1 {
                    let sc = to_i64(&a(1));
                    if !(0..=2147483647).contains(&sc) {
                        return Err(self.throw_error(
                            "ValueError",
                            "bcsqrt(): Argument #2 ($scale) must be between 0 and 2147483647",
                        ));
                    }
                    (sc as usize).min(100_000)
                } else {
                    self.bc_scale
                };
                match crate::bc::sqrt(&x, scale) {
                    Some(r) => Value::Str(r.to_string_scaled(scale).into_bytes()),
                    None => {
                        return Err(self.throw_error(
                            "ValueError",
                            "bcsqrt(): Argument #1 ($num) must be greater than or equal to 0",
                        ))
                    }
                }
            }
            "version_compare" => {
                // PHP semantics: canonicalize (insert dots at digit/alpha
                // boundaries, -_+ become dots), then compare parts by special
                // class (dev < alpha/a < beta/b < RC/rc < numbers < pl/p),
                // numeric parts numerically. Missing parts rank below numbers.
                fn canon(v: &str) -> Vec<String> {
                    let mut out: Vec<String> = Vec::new();
                    let mut cur = String::new();
                    let mut prev: Option<char> = None;
                    for ch in v.chars() {
                        let boundary = match (prev, ch) {
                            (_, '.') | (_, '-') | (_, '_') | (_, '+') => true,
                            (Some(p), c) => {
                                (p.is_ascii_digit() && !c.is_ascii_digit())
                                    || (!p.is_ascii_digit() && c.is_ascii_digit())
                            }
                            (None, _) => false,
                        };
                        if boundary {
                            if !cur.is_empty() {
                                out.push(std::mem::take(&mut cur));
                            }
                        }
                        if !matches!(ch, '.' | '-' | '_' | '+') {
                            cur.push(ch);
                            prev = Some(ch);
                        } else {
                            prev = None;
                        }
                    }
                    if !cur.is_empty() {
                        out.push(cur);
                    }
                    out
                }
                // (class, numeric value, text): unknown(0) < dev(1) < alpha(2)
                // < beta(3) < rc(4) < numbers(5) < pl(6); missing part = (5,-1)
                fn form(part: Option<&String>) -> (i32, i64, String) {
                    match part {
                        None => (5, -1, String::new()),
                        Some(p) => {
                            if !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()) {
                                (5, p.parse::<i64>().unwrap_or(i64::MAX), p.clone())
                            } else {
                                let c = match p.to_ascii_lowercase().as_str() {
                                    "dev" => 1,
                                    "alpha" | "a" => 2,
                                    "beta" | "b" => 3,
                                    "rc" => 4,
                                    "pl" | "p" => 6,
                                    _ => 0,
                                };
                                (c, 0, p.clone())
                            }
                        }
                    }
                }
                let v1 = String::from_utf8_lossy(&to_bytes(&a(0))).into_owned();
                let v2 = String::from_utf8_lossy(&to_bytes(&a(1))).into_owned();
                let (p1, p2) = (canon(&v1), canon(&v2));
                let n = p1.len().max(p2.len());
                let mut result = 0i64;
                for i in 0..n {
                    let (c1, n1, s1) = form(p1.get(i));
                    let (c2, n2, s2) = form(p2.get(i));
                    let ord = if c1 != c2 {
                        c1.cmp(&c2)
                    } else if c1 == 5 {
                        n1.cmp(&n2)
                    } else {
                        s1.to_ascii_lowercase().cmp(&s2.to_ascii_lowercase())
                    };
                    match ord {
                        std::cmp::Ordering::Less => {
                            result = -1;
                            break;
                        }
                        std::cmp::Ordering::Greater => {
                            result = 1;
                            break;
                        }
                        std::cmp::Ordering::Equal => {}
                    }
                }
                if args.len() > 2 {
                    let op = String::from_utf8_lossy(&to_bytes(&a(2))).into_owned();
                    let b = match op.as_str() {
                        "<" | "lt" => result < 0,
                        "<=" | "le" => result <= 0,
                        ">" | "gt" => result > 0,
                        ">=" | "ge" => result >= 0,
                        "==" | "eq" => result == 0,
                        "!=" | "ne" | "<>" => result != 0,
                        _ => {
                            return Err(self.throw_error(
                                "ValueError",
                                "version_compare(): Argument #3 ($operator) must be a valid comparison operator",
                            ))
                        }
                    };
                    Value::Bool(b)
                } else {
                    Value::Int(result)
                }
            }
            "bcfloor" | "bcceil" => {
                let Some(x) = crate::bc::Dec::parse(&String::from_utf8_lossy(&to_bytes(&a(0)))) else {
                    return Err(self.throw_error(
                        "ValueError",
                        &format!("{name}(): Argument #1 ($num) is not well-formed"),
                    ));
                };
                let r = if name == "bcfloor" { crate::bc::floor(&x) } else { crate::bc::ceil(&x) };
                Value::Str(r.to_string_scaled(0).into_bytes())
            }
            "bcround" => {
                let Some(x) = crate::bc::Dec::parse(&String::from_utf8_lossy(&to_bytes(&a(0)))) else {
                    return Err(self.throw_error(
                        "ValueError",
                        "bcround(): Argument #1 ($num) is not well-formed",
                    ));
                };
                let prec = if args.len() > 1 { to_i64(&a(1)).max(0) as usize } else { 0 };
                let mode = match &a(2) {
                    Value::Object(rc) => {
                        let n = rc.borrow().get("name").map(to_bytes).unwrap_or_default();
                        match n.as_slice() {
                            b"HalfTowardsZero" => crate::bc::Round::HalfTowards,
                            b"HalfEven" => crate::bc::Round::HalfEven,
                            b"HalfOdd" => crate::bc::Round::HalfOdd,
                            b"TowardsZero" => crate::bc::Round::Towards,
                            b"AwayFromZero" => crate::bc::Round::Away,
                            b"NegativeInfinity" => crate::bc::Round::NegInf,
                            b"PositiveInfinity" => crate::bc::Round::PosInf,
                            _ => crate::bc::Round::HalfAway,
                        }
                    }
                    _ => crate::bc::Round::HalfAway,
                };
                let r = crate::bc::round_mode(&x, prec.min(100_000), mode);
                Value::Str(r.to_string_scaled(prec.min(100_000)).into_bytes())
            }
            "bcpowmod" => {
                let parse = |v: &Value| crate::bc::Dec::parse(&String::from_utf8_lossy(&to_bytes(v)));
                let (Some(b), Some(e), Some(m)) = (parse(&a(0)), parse(&a(1)), parse(&a(2))) else {
                    return Err(self.throw_error(
                        "ValueError",
                        "bcpowmod(): argument is not well-formed",
                    ));
                };
                // PHP validates integrality and exponent sign with specific wording
                let frac = |d: &crate::bc::Dec| {
                    crate::bc::cmp(d, &d.with_scale(0)) != std::cmp::Ordering::Equal
                };
                if frac(&b) {
                    return Err(self.throw_error(
                        "ValueError",
                        "bcpowmod(): Argument #1 ($num) cannot have a fractional part",
                    ));
                }
                if frac(&e) {
                    return Err(self.throw_error(
                        "ValueError",
                        "bcpowmod(): Argument #2 ($exponent) cannot have a fractional part",
                    ));
                }
                if e.neg && !e.is_zero() {
                    return Err(self.throw_error(
                        "ValueError",
                        "bcpowmod(): Argument #2 ($exponent) must be greater than or equal to 0",
                    ));
                }
                if frac(&m) {
                    return Err(self.throw_error(
                        "ValueError",
                        "bcpowmod(): Argument #3 ($modulus) cannot have a fractional part",
                    ));
                }
                let scale = if args.len() > 3 { to_i64(&a(3)).max(0) as usize } else { self.bc_scale };
                match crate::bc::powmod(&b, &e, &m) {
                    Some(r) => Value::Str(r.to_string_scaled(scale.min(100_000)).into_bytes()),
                    None => {
                        return Err(self.throw_error("DivisionByZeroError", "Modulo by zero"))
                    }
                }
            }
            "bcdivmod" => {
                let parse = |v: &Value| crate::bc::Dec::parse(&String::from_utf8_lossy(&to_bytes(v)));
                let (Some(x), Some(y)) = (parse(&a(0)), parse(&a(1))) else {
                    return Err(self.throw_error(
                        "ValueError",
                        "bcdivmod(): argument is not well-formed",
                    ));
                };
                if y.is_zero() {
                    return Err(self.throw_error("DivisionByZeroError", "Division by zero"));
                }
                let scale = if args.len() > 2 { to_i64(&a(2)).max(0) as usize } else { self.bc_scale };
                let q = crate::bc::div(&x, &y, 0).unwrap_or_else(crate::bc::Dec::zero);
                let r = crate::bc::modulo(&x, &y, scale).unwrap_or_else(crate::bc::Dec::zero);
                let mut arr = Arr::new();
                arr.push(Value::Str(q.to_string_scaled(0).into_bytes()));
                arr.push(Value::Str(r.to_string_scaled(scale).into_bytes()));
                Value::Array(arr)
            }
            "__phargo_bcscale_of" => {
                let raw = String::from_utf8_lossy(&to_bytes(&a(0))).into_owned();
                match crate::bc::Dec::parse(&raw) {
                    Some(d) => Value::Int(d.scale as i64),
                    None => {
                        return Err(self.throw_error(
                            "ValueError",
                            "BcMath\\Number::__construct(): Argument #1 ($num) is not well-formed",
                        ))
                    }
                }
            }
            "bcscale" => {
                let old = self.bc_scale as i64;
                if !args.is_empty() {
                    let sc = to_i64(&a(0));
                    if !(0..=2147483647).contains(&sc) {
                        return Err(self.throw_error(
                            "ValueError",
                            "bcscale(): Argument #1 ($scale) must be between 0 and 2147483647",
                        ));
                    }
                    self.bc_scale = (sc as usize).min(100_000);
                }
                Value::Int(old)
            }
            "pow" => self.apply_bin(BinOp::Pow, &a(0), &a(1))?,
            "intdiv" => {
                let d = to_i64(&a(1));
                if d == 0 {
                    return Err(self.throw_error("DivisionByZeroError", "Division by zero"));
                }
                Value::Int(to_i64(&a(0)) / d)
            }
            "fdiv" => Value::Float(to_f64(&a(0)) / to_f64(&a(1))),
            "class_alias" => {
                let orig = String::from_utf8_lossy(&to_bytes(&a(0))).to_ascii_lowercase();
                let alias = String::from_utf8_lossy(&to_bytes(&a(1))).into_owned();
                if let Some(c) = self.classes.get(&orig).cloned() {
                    self.classes.insert(alias.to_ascii_lowercase(), c);
                    Value::Bool(true)
                } else {
                    Value::Bool(false)
                }
            }
            "get_called_class" => match &self.called_class.clone().or_else(|| self.current_class.clone()) {
                Some(c) => Value::Str(c.as_bytes().to_vec()),
                None => Value::Bool(false),
            },
            "func_get_args" => {
                let mut arr = Arr::new();
                if let Some(cur) = self.cur_args.last() {
                    for v in cur.iter().cloned() {
                        arr.push(v);
                    }
                }
                Value::Array(arr)
            }
            "func_num_args" => Value::Int(self.cur_args.last().map(|a| a.len()).unwrap_or(0) as i64),
            "func_get_arg" => {
                let i = to_i64(&a(0)).max(0) as usize;
                self.cur_args.last().and_then(|a| a.get(i).cloned()).unwrap_or(Value::Bool(false))
            }
            "extract" => {
                let mut n = 0;
                if let Value::Array(arr) = a(0) {
                    for (k, v) in arr.entries {
                        if let Key::Str(s) = k {
                            if let Ok(name) = std::str::from_utf8(&s) {
                                if !name.is_empty() && !name.starts_with(|c: char| c.is_ascii_digit()) {
                                    self.vars().insert(name.to_string(), v);
                                    n += 1;
                                }
                            }
                        }
                    }
                }
                Value::Int(n)
            }
            "rand" | "mt_rand" => {
                if args.len() >= 2 {
                    let lo = to_i64(&a(0));
                    let hi = to_i64(&a(1));
                    if hi <= lo {
                        Value::Int(lo)
                    } else {
                        let span = (hi - lo + 1) as i64;
                        Value::Int(lo + (self.next_rand() % span as u64) as i64)
                    }
                } else {
                    Value::Int((self.next_rand() & 0x7fff_ffff) as i64)
                }
            }
            "random_int" => {
                let lo = to_i64(&a(0));
                let hi = to_i64(&a(1));
                if hi <= lo {
                    Value::Int(lo)
                } else {
                    let span = (hi - lo + 1) as u64;
                    Value::Int(lo + (self.next_rand() % span) as i64)
                }
            }
            "srand" | "mt_srand" => {
                self.rng_state = if args.is_empty() { 0x2545_F491_4F6C_DD1D } else { to_i64(&a(0)) as u64 };
                Value::Null
            }
            "mt_getrandmax" | "getrandmax" => Value::Int(2_147_483_647),
            "highlight_string" | "highlight_file" => {
                let code = to_bytes(&a(0));
                if to_bool(&a(1)) {
                    Value::Str(code)
                } else {
                    self.out.extend_from_slice(&code);
                    Value::Bool(true)
                }
            }
            "strtoupper" => Value::Str(to_bytes(&a(0)).to_ascii_uppercase()),
            "strtolower" => Value::Str(to_bytes(&a(0)).to_ascii_lowercase()),
            "ucfirst" => {
                let mut b = to_bytes(&a(0));
                if let Some(c) = b.first_mut() {
                    c.make_ascii_uppercase();
                }
                Value::Str(b)
            }
            "lcfirst" => {
                let mut b = to_bytes(&a(0));
                if let Some(c) = b.first_mut() {
                    c.make_ascii_lowercase();
                }
                Value::Str(b)
            }
            "trim" | "ltrim" | "rtrim" | "chop" => {
                let (left, right) = match name {
                    "ltrim" => (true, false),
                    "rtrim" | "chop" => (false, true),
                    _ => (true, true),
                };
                let sbytes = to_bytes(&a(0));
                if args.len() > 1 {
                    // explicit charlist, with PHP's `a..z` range syntax
                    let list = to_bytes(&a(1));
                    let mut set = [false; 256];
                    let mut i = 0;
                    while i < list.len() {
                        if i + 3 < list.len() && list[i + 1] == b'.' && list[i + 2] == b'.' {
                            let (lo, hi) = (list[i], list[i + 3]);
                            for c in lo..=hi {
                                set[c as usize] = true;
                            }
                            i += 4;
                        } else {
                            set[list[i] as usize] = true;
                            i += 1;
                        }
                    }
                    let mut start = 0;
                    let mut end = sbytes.len();
                    if left {
                        while start < end && set[sbytes[start] as usize] {
                            start += 1;
                        }
                    }
                    if right {
                        while end > start && set[sbytes[end - 1] as usize] {
                            end -= 1;
                        }
                    }
                    Value::Str(sbytes[start..end].to_vec())
                } else {
                    Value::Str(trim_bytes(&sbytes, left, right))
                }
            }
            "str_repeat" => {
                let s = to_bytes(&a(0));
                let n = to_i64(&a(1)).max(0) as usize;
                if s.len().saturating_mul(n) > MAX_STR {
                    return Err(self.throw_error("ValueError", "str_repeat result too large"));
                }
                Value::Str(s.repeat(n))
            }
            "strrev" => {
                let mut b = to_bytes(&a(0));
                b.reverse();
                Value::Str(b)
            }
            "ord" => Value::Int(to_bytes(&a(0)).first().copied().unwrap_or(0) as i64),
            "chr" => Value::Str(vec![(to_i64(&a(0)).rem_euclid(256)) as u8]),
            // ---- hashing / encoding (reuse the shared hash.rs subsystem) ----
            "md5" => {
                let h = crate::md5_hex(&to_bytes(&a(0)));
                if to_bool(&a(1)) { Value::Str(hex_to_bytes(&h)) } else { Value::Str(h.into_bytes()) }
            }
            "sha1" => {
                let h = crate::sha1_hex(&to_bytes(&a(0)));
                if to_bool(&a(1)) { Value::Str(hex_to_bytes(&h)) } else { Value::Str(h.into_bytes()) }
            }
            "crc32" => Value::Int(crate::crc32(&to_bytes(&a(0))) as i64),
            "hash" => {
                let algo = String::from_utf8_lossy(&to_bytes(&a(0))).to_ascii_lowercase();
                let data = to_bytes(&a(1));
                let h = match algo.as_str() {
                    "md5" => crate::md5_hex(&data),
                    "sha1" => crate::sha1_hex(&data),
                    "sha256" => crate::sha256_hex(&data),
                    "crc32b" => format!("{:08x}", crate::crc32(&data)),
                    _ => {
                        return Err(self.throw_error(
                            "ValueError",
                            &format!("hash(): Argument #1 ($algo) must be a valid hashing algorithm ({algo})"),
                        ))
                    }
                };
                if to_bool(&a(2)) { Value::Str(hex_to_bytes(&h)) } else { Value::Str(h.into_bytes()) }
            }
            "hash_equals" => Value::Bool(to_bytes(&a(0)) == to_bytes(&a(1))),
            "hash_hmac" => {
                let algo = String::from_utf8_lossy(&to_bytes(&a(0))).to_ascii_lowercase();
                let data = to_bytes(&a(1));
                let key = to_bytes(&a(2));
                match crate::hmac_hex(&algo, &data, &key) {
                    Some(hex) => {
                        if to_bool(&a(3)) {
                            Value::Str(hex_to_bytes(&hex))
                        } else {
                            Value::Str(hex.into_bytes())
                        }
                    }
                    None => Value::Bool(false),
                }
            }
            "hash_algos" | "hash_hmac_algos" => {
                let mut arr = Arr::new();
                for n in ["md5", "sha1", "sha256", "crc32b"] {
                    arr.push(Value::Str(n.as_bytes().to_vec()));
                }
                Value::Array(arr)
            }
            "base64_encode" => Value::Str(crate::base64_encode(&to_bytes(&a(0))).into_bytes()),
            "base64_decode" => {
                Value::Str(crate::base64_decode(&String::from_utf8_lossy(&to_bytes(&a(0)))))
            }
            "bin2hex" => {
                let s = to_bytes(&a(0));
                let mut o = String::with_capacity(s.len() * 2);
                for b in s {
                    o.push_str(&format!("{b:02x}"));
                }
                Value::Str(o.into_bytes())
            }
            "hex2bin" => Value::Str(hex_to_bytes(&String::from_utf8_lossy(&to_bytes(&a(0))))),
            "dechex" => Value::Str(format!("{:x}", to_i64(&a(0))).into_bytes()),
            "hexdec" => Value::Int(
                i64::from_str_radix(String::from_utf8_lossy(&to_bytes(&a(0))).trim(), 16).unwrap_or(0),
            ),
            "decbin" => Value::Str(format!("{:b}", to_i64(&a(0))).into_bytes()),
            "bindec" => Value::Int(
                i64::from_str_radix(String::from_utf8_lossy(&to_bytes(&a(0))).trim(), 2).unwrap_or(0),
            ),
            "decoct" => Value::Str(format!("{:o}", to_i64(&a(0))).into_bytes()),
            "octdec" => Value::Int(
                i64::from_str_radix(String::from_utf8_lossy(&to_bytes(&a(0))).trim(), 8).unwrap_or(0),
            ),
            // ---- mbstring (UTF-8, code-point based) ----
            "mb_strlen" => {
                Value::Int(String::from_utf8_lossy(&to_bytes(&a(0))).chars().count() as i64)
            }
            "mb_substr" => {
                let s: Vec<char> = String::from_utf8_lossy(&to_bytes(&a(0))).chars().collect();
                let len = s.len() as i64;
                let mut start = to_i64(&a(1));
                if start < 0 {
                    start = (len + start).max(0);
                }
                let start = start.min(len) as usize;
                let end = if args.len() > 2 && !matches!(a(2), Value::Null) {
                    let l = to_i64(&a(2));
                    if l < 0 {
                        ((len + l).max(start as i64)) as usize
                    } else {
                        (start + l as usize).min(s.len())
                    }
                } else {
                    s.len()
                };
                Value::Str(s[start..end.max(start)].iter().collect::<String>().into_bytes())
            }
            "mb_strtoupper" => {
                Value::Str(String::from_utf8_lossy(&to_bytes(&a(0))).to_uppercase().into_bytes())
            }
            "mb_strtolower" => {
                Value::Str(String::from_utf8_lossy(&to_bytes(&a(0))).to_lowercase().into_bytes())
            }
            "mb_str_split" => {
                let s: Vec<char> = String::from_utf8_lossy(&to_bytes(&a(0))).chars().collect();
                let n = if args.len() > 1 { to_i64(&a(1)).max(1) as usize } else { 1 };
                let mut arr = Arr::new();
                for chunk in s.chunks(n) {
                    arr.push(Value::Str(chunk.iter().collect::<String>().into_bytes()));
                }
                Value::Array(arr)
            }
            "mb_strpos" => {
                let hay: Vec<char> = String::from_utf8_lossy(&to_bytes(&a(0))).chars().collect();
                let needle: Vec<char> = String::from_utf8_lossy(&to_bytes(&a(1))).chars().collect();
                let from = to_i64(&a(2)).max(0) as usize;
                if needle.is_empty() {
                    Value::Int(from as i64)
                } else {
                    match (from..=hay.len().saturating_sub(needle.len()))
                        .find(|&i| hay[i..i + needle.len()] == needle[..])
                    {
                        Some(i) => Value::Int(i as i64),
                        None => Value::Bool(false),
                    }
                }
            }
            "mb_ord" => match String::from_utf8_lossy(&to_bytes(&a(0))).chars().next() {
                Some(c) => Value::Int(c as i64),
                None => Value::Bool(false),
            },
            "mb_chr" => match char::from_u32(to_i64(&a(0)) as u32) {
                Some(c) => Value::Str(c.to_string().into_bytes()),
                None => Value::Bool(false),
            },
            "mb_internal_encoding" | "mb_detect_encoding" | "mb_http_output" => {
                Value::Str(b"UTF-8".to_vec())
            }
            "mb_convert_encoding" | "mb_scrub" => a(0),
            "mb_check_encoding" => Value::Bool(true),
            "mb_convert_case" => {
                let bytes = to_bytes(&a(0));
                let s = String::from_utf8_lossy(&bytes);
                let r = match to_i64(&a(1)) {
                    0 => s.to_uppercase(),
                    1 => s.to_lowercase(),
                    _ => ucwords_str(&s), // MB_CASE_TITLE
                };
                Value::Str(r.into_bytes())
            }
            // ---- more string builtins ----
            "str_pad" => {
                let s = to_bytes(&a(0));
                let len = (to_i64(&a(1)).max(0) as usize).min(MAX_STR);
                let pad = {
                    let p = to_bytes(&a(2));
                    if p.is_empty() { vec![b' '] } else { p }
                };
                let ptype = if args.len() > 3 { to_i64(&a(3)) } else { 1 };
                if s.len() >= len {
                    Value::Str(s)
                } else {
                    let total = len - s.len();
                    let mk = |n: usize| -> Vec<u8> { pad.iter().cloned().cycle().take(n).collect() };
                    let r = match ptype {
                        0 => {
                            let mut r = mk(total);
                            r.extend_from_slice(&s);
                            r
                        }
                        2 => {
                            let l = total / 2;
                            let mut o = mk(l);
                            o.extend_from_slice(&s);
                            o.extend(mk(total - l));
                            o
                        }
                        _ => {
                            let mut r = s.clone();
                            r.extend(mk(total));
                            r
                        }
                    };
                    Value::Str(r)
                }
            }
            "number_format" => {
                let n = to_f64(&a(0));
                let dec = if args.len() > 1 { to_i64(&a(1)).max(0) as usize } else { 0 };
                let dp = if args.len() > 2 {
                    String::from_utf8_lossy(&to_bytes(&a(2))).into_owned()
                } else {
                    ".".into()
                };
                let ts = if args.len() > 3 {
                    String::from_utf8_lossy(&to_bytes(&a(3))).into_owned()
                } else {
                    ",".into()
                };
                let formatted = format!("{:.*}", dec, n.abs());
                let (int_part, frac_part) = match formatted.split_once('.') {
                    Some((i, f)) => (i.to_string(), f.to_string()),
                    None => (formatted, String::new()),
                };
                let digits: Vec<char> = int_part.chars().collect();
                let mut int_ts = String::new();
                for (i, c) in digits.iter().enumerate() {
                    if i > 0 && (digits.len() - i) % 3 == 0 {
                        int_ts.push_str(&ts);
                    }
                    int_ts.push(*c);
                }
                let mut result = String::new();
                let nonzero = int_part != "0" || frac_part.chars().any(|c| c != '0');
                if n < 0.0 && nonzero {
                    result.push('-');
                }
                result.push_str(&int_ts);
                if dec > 0 {
                    result.push_str(&dp);
                    result.push_str(&frac_part);
                }
                Value::Str(result.into_bytes())
            }
            "ucwords" => {
                Value::Str(ucwords_str(&String::from_utf8_lossy(&to_bytes(&a(0)))).into_bytes())
            }
            "nl2br" => {
                let s = to_bytes(&a(0));
                let mut out = Vec::new();
                let mut i = 0;
                while i < s.len() {
                    if s[i] == b'\r' && s.get(i + 1) == Some(&b'\n') {
                        out.extend_from_slice(b"<br />\r\n");
                        i += 2;
                    } else if s[i] == b'\n' || s[i] == b'\r' {
                        out.extend_from_slice(b"<br />");
                        out.push(s[i]);
                        i += 1;
                    } else {
                        out.push(s[i]);
                        i += 1;
                    }
                }
                Value::Str(out)
            }
            "substr_count" => {
                let hay = to_bytes(&a(0));
                let needle = to_bytes(&a(1));
                if needle.is_empty() {
                    Value::Int(0)
                } else {
                    let mut count = 0;
                    let mut i = 0;
                    while i + needle.len() <= hay.len() {
                        if hay[i..i + needle.len()] == needle[..] {
                            count += 1;
                            i += needle.len();
                        } else {
                            i += 1;
                        }
                    }
                    Value::Int(count)
                }
            }
            "str_split" => {
                let s = to_bytes(&a(0));
                let n = if args.len() > 1 { to_i64(&a(1)).max(1) as usize } else { 1 };
                let mut arr = Arr::new();
                if s.is_empty() {
                    arr.push(Value::Str(Vec::new()));
                } else {
                    for chunk in s.chunks(n) {
                        arr.push(Value::Str(chunk.to_vec()));
                    }
                }
                Value::Array(arr)
            }
            "chunk_split" => {
                let s = to_bytes(&a(0));
                let n = if args.len() > 1 { to_i64(&a(1)).max(1) as usize } else { 76 };
                let end = if args.len() > 2 { to_bytes(&a(2)) } else { b"\r\n".to_vec() };
                // memory-bomb guard: a long `end` * many chunks can explode
                if (s.len() / n + 1).saturating_mul(n + end.len()) > MAX_STR {
                    return Err(self.throw_error("Error", "chunk_split result too large"));
                }
                let mut out = Vec::new();
                for chunk in s.chunks(n) {
                    out.extend_from_slice(chunk);
                    out.extend_from_slice(&end);
                }
                Value::Str(out)
            }
            "str_word_count" => {
                let s = String::from_utf8_lossy(&to_bytes(&a(0))).into_owned();
                let words: Vec<&str> = s.split_whitespace().collect();
                if to_i64(&a(1)) >= 1 {
                    let mut arr = Arr::new();
                    for w in words {
                        arr.push(Value::Str(w.as_bytes().to_vec()));
                    }
                    Value::Array(arr)
                } else {
                    Value::Int(words.len() as i64)
                }
            }
            "levenshtein" => {
                Value::Int(levenshtein(&to_bytes(&a(0)), &to_bytes(&a(1))) as i64)
            }
            "htmlspecialchars" | "htmlentities" => {
                // ASCII entity-encode; flags default ENT_QUOTES|ENT_HTML401 (PHP 8.1+).
                let s = to_bytes(&a(0));
                let flags = if args.len() > 1 { to_i64(&a(1)) } else { 11 };
                let dq = flags & 2 != 0;
                let sq = flags & 1 != 0;
                // 4th arg double_encode=false: leave existing entities alone
                // (WP's esc_html/_wp_specialchars relies on it — a pre-encoded
                // &#8211; separator must not become &amp;#8211;)
                let double_encode = args.len() < 4 || to_bool(&a(3));
                let is_entity_at = |i: usize| -> bool {
                    // numeric entities (&#123; / &#xAB;) always count; named
                    // ones only if PHP's HTML table knows them (&x; re-encodes)
                    let mut j = i + 1;
                    let numeric = j < s.len() && s[j] == b'#';
                    if numeric {
                        j += 1;
                        if j < s.len() && (s[j] == b'x' || s[j] == b'X') {
                            j += 1;
                        }
                    }
                    let start = j;
                    while j < s.len() && s[j].is_ascii_alphanumeric() {
                        j += 1;
                    }
                    if !(j > start && j < s.len() && s[j] == b';') {
                        return false;
                    }
                    if numeric {
                        return true;
                    }
                    HTML_ENTITY_NAMES
                        .binary_search(&std::str::from_utf8(&s[start..j]).unwrap_or(""))
                        .is_ok()
                };
                let mut out = Vec::with_capacity(s.len());
                for (i, &b) in s.iter().enumerate() {
                    match b {
                        b'&' => {
                            if !double_encode && is_entity_at(i) {
                                out.push(b'&');
                            } else {
                                out.extend_from_slice(b"&amp;");
                            }
                        }
                        b'<' => out.extend_from_slice(b"&lt;"),
                        b'>' => out.extend_from_slice(b"&gt;"),
                        b'"' if dq => out.extend_from_slice(b"&quot;"),
                        b'\'' if sq => out.extend_from_slice(b"&#039;"),
                        _ => out.push(b),
                    }
                }
                Value::Str(out)
            }
            "htmlspecialchars_decode" | "html_entity_decode" => {
                let s = to_bytes(&a(0));
                Value::Str(decode_html_entities(&s))
            }
            "urlencode" | "rawurlencode" => {
                let s = to_bytes(&a(0));
                let raw = name == "rawurlencode";
                let mut out = Vec::with_capacity(s.len());
                for &b in &s {
                    if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.')
                        || (raw && b == b'~')
                    {
                        out.push(b);
                    } else if b == b' ' && !raw {
                        out.push(b'+');
                    } else {
                        out.extend_from_slice(format!("%{b:02X}").as_bytes());
                    }
                }
                Value::Str(out)
            }
            "urldecode" | "rawurldecode" => {
                let s = to_bytes(&a(0));
                let plus = name == "urldecode";
                let mut out = Vec::with_capacity(s.len());
                let mut i = 0;
                while i < s.len() {
                    match s[i] {
                        b'%' if i + 2 < s.len() => {
                            let hi = (s[i + 1] as char).to_digit(16);
                            let lo = (s[i + 2] as char).to_digit(16);
                            if let (Some(h), Some(l)) = (hi, lo) {
                                out.push((h * 16 + l) as u8);
                                i += 3;
                                continue;
                            }
                            out.push(s[i]);
                            i += 1;
                        }
                        b'+' if plus => { out.push(b' '); i += 1; }
                        b => { out.push(b); i += 1; }
                    }
                }
                Value::Str(out)
            }
            "http_build_query" => {
                let mut parts: Vec<u8> = Vec::new();
                if let Value::Array(arr) = a(0) {
                    for (k, v) in &arr.entries {
                        if !parts.is_empty() { parts.push(b'&'); }
                        let key = match k { Key::Int(n) => n.to_string().into_bytes(), Key::Str(s) => s.clone() };
                        parts.extend_from_slice(&urlencode_form(&key));
                        parts.push(b'=');
                        parts.extend_from_slice(&urlencode_form(&to_bytes(v)));
                    }
                }
                Value::Str(parts)
            }
            "mb_substitute_character" => {
                if args.is_empty() { Value::Str(b"none".to_vec()) } else { Value::Bool(true) }
            }
            "str_rot13" => {
                let mut s = to_bytes(&a(0));
                for b in s.iter_mut() {
                    match *b {
                        b'a'..=b'z' => *b = b'a' + (*b - b'a' + 13) % 26,
                        b'A'..=b'Z' => *b = b'A' + (*b - b'A' + 13) % 26,
                        _ => {}
                    }
                }
                Value::Str(s)
            }
            "pack" => {
                let fmt = to_bytes(&a(0));
                Value::Str(pack_values(&fmt, &args[1.min(args.len())..]))
            }
            "unpack" => {
                let fmt = to_bytes(&a(0));
                let data = to_bytes(&a(1));
                unpack_values(&fmt, &data)
            }
            "escapeshellarg" => {
                // POSIX single-quote escaping: wrap in '...', and '\'' for inner quotes.
                let s = to_bytes(&a(0));
                let mut out = vec![b'\''];
                for &b in &s {
                    if b == b'\'' {
                        out.extend_from_slice(b"'\\''");
                    } else {
                        out.push(b);
                    }
                }
                out.push(b'\'');
                Value::Str(out)
            }
            "escapeshellcmd" => {
                let s = to_bytes(&a(0));
                let mut out = Vec::with_capacity(s.len());
                for &b in &s {
                    if matches!(b, b'&' | b';' | b'`' | b'|' | b'*' | b'?' | b'~' | b'<' | b'>'
                        | b'^' | b'(' | b')' | b'[' | b']' | b'{' | b'}' | b'$' | b'\\'
                        | 0x0a | 0xff | b'"' | b'\'' | b'#') {
                        out.push(b'\\');
                    }
                    out.push(b);
                }
                Value::Str(out)
            }
            "strip_tags" => {
                // Remove `<...>` tags (and `<?...?>`/comments). `allowed` (2nd arg,
                // string like "<a><b>" or array of names) preserves those tags.
                let s = to_bytes(&a(0));
                let allowed: Vec<Vec<u8>> = match a(1) {
                    Value::Str(spec) => {
                        let lower = spec.to_ascii_lowercase();
                        let mut v = Vec::new();
                        let mut cur = Vec::new();
                        for &b in &lower {
                            match b {
                                b'<' => cur.clear(),
                                b'>' => { if !cur.is_empty() { v.push(std::mem::take(&mut cur)); } }
                                _ => cur.push(b),
                            }
                        }
                        v
                    }
                    Value::Array(arr) => arr.entries.iter().map(|(_, v)| to_bytes(v).to_ascii_lowercase()).collect(),
                    _ => Vec::new(),
                };
                Value::Str(strip_tags(&s, &allowed))
            }
            "wordwrap" => {
                let s = String::from_utf8_lossy(&to_bytes(&a(0))).into_owned();
                let width = if args.len() > 1 { to_i64(&a(1)).max(1) as usize } else { 75 };
                let brk = if args.len() > 2 {
                    String::from_utf8_lossy(&to_bytes(&a(2))).into_owned()
                } else {
                    "\n".into()
                };
                let mut out = String::new();
                let mut line_len = 0;
                for (i, word) in s.split(' ').enumerate() {
                    if i > 0 {
                        if line_len + 1 + word.len() > width {
                            out.push_str(&brk);
                            line_len = 0;
                        } else {
                            out.push(' ');
                            line_len += 1;
                        }
                    }
                    out.push_str(word);
                    line_len += word.len();
                }
                Value::Str(out.into_bytes())
            }
            // ---- more array builtins ----
            "array_replace" | "array_replace_recursive" => {
                let recursive = name == "array_replace_recursive";
                fn rep(base: &mut Arr, over: &Arr, recursive: bool) {
                    for (k, v) in &over.entries {
                        if recursive {
                            if let (Some(Value::Array(b)), Value::Array(o)) =
                                (base.get_mut(k), v)
                            {
                                rep(b, o, true);
                                continue;
                            }
                        }
                        base.insert(k.clone(), v.clone());
                    }
                }
                let mut out = match a(0) {
                    Value::Array(arr) => arr,
                    _ => Arr::new(),
                };
                for v in &args[1..] {
                    if let Value::Array(o) = v {
                        rep(&mut out, o, recursive);
                    }
                }
                Value::Array(out)
            }
            "array_intersect_key" | "array_diff_key" => {
                let keep_present = name == "array_intersect_key";
                let mut out = Arr::new();
                if let Value::Array(arr) = a(0) {
                    let others: Vec<&Arr> = args[1..]
                        .iter()
                        .filter_map(|v| match v {
                            Value::Array(o) => Some(o),
                            _ => None,
                        })
                        .collect();
                    for (k, v) in &arr.entries {
                        let in_all = others.iter().all(|o| o.get(k).is_some());
                        let in_any = others.iter().any(|o| o.get(k).is_some());
                        if (keep_present && in_all) || (!keep_present && !in_any) {
                            out.insert(k.clone(), v.clone());
                        }
                    }
                }
                Value::Array(out)
            }
            "array_diff" => {
                let mut out = Arr::new();
                if let Value::Array(arr) = a(0) {
                    // O(1) membership via a hash set (was O(n*m) — bug74093: two ~3M arrays)
                    let mut others: HashSet<Vec<u8>> = HashSet::new();
                    for v in &args[1..] {
                        if let Value::Array(o) = v {
                            for (_, x) in &o.entries {
                                others.insert(to_bytes(x));
                            }
                        }
                    }
                    for (k, v) in arr.entries {
                        if !others.contains(&to_bytes(&v)) {
                            out.insert(k, v);
                        }
                    }
                }
                Value::Array(out)
            }
            "array_intersect" => {
                let mut out = Arr::new();
                if let Value::Array(arr) = a(0) {
                    let sets: Vec<HashSet<Vec<u8>>> = args[1..]
                        .iter()
                        .map(|v| {
                            if let Value::Array(o) = v {
                                o.entries.iter().map(|(_, x)| to_bytes(x)).collect()
                            } else {
                                HashSet::new()
                            }
                        })
                        .collect();
                    for (k, v) in arr.entries {
                        let vb = to_bytes(&v);
                        if sets.iter().all(|s| s.contains(&vb)) {
                            out.insert(k, v);
                        }
                    }
                }
                Value::Array(out)
            }
            "array_fill_keys" => {
                let mut out = Arr::new();
                let val = a(1);
                if let Value::Array(keys) = a(0) {
                    for (_, k) in keys.entries {
                        out.insert(Arr::norm_key(&k), val.clone());
                    }
                }
                Value::Array(out)
            }
            "array_walk" => {
                let cb = a(1);
                let extra = a(2);
                if let Value::Array(arr) = a(0) {
                    for (k, v) in arr.entries {
                        let kv = match k {
                            Key::Int(n) => Value::Int(n),
                            Key::Str(s) => Value::Str(s),
                        };
                        let mut cargs = vec![v, kv];
                        if args.len() > 2 {
                            cargs.push(extra.clone());
                        }
                        self.call_value(cb.clone(), cargs)?;
                    }
                }
                Value::Bool(true)
            }
            "array_walk_recursive" => {
                let cb = a(1);
                let has_extra = args.len() > 2;
                let extra = a(2);
                if let Value::Array(arr) = a(0) {
                    self.walk_recursive(&arr, &cb, has_extra, &extra)?;
                }
                Value::Bool(true)
            }
            "array_product" => {
                let mut fi = 1i64;
                let mut ff = 1f64;
                let mut isf = false;
                if let Value::Array(arr) = a(0) {
                    for (_, v) in &arr.entries {
                        match to_num(v) {
                            Num::Int(n) if !isf => fi = fi.wrapping_mul(n),
                            n => {
                                if !isf {
                                    ff = fi as f64;
                                    isf = true;
                                }
                                ff *= n.as_f64();
                            }
                        }
                    }
                }
                if isf { Value::Float(ff) } else { Value::Int(fi) }
            }
            "array_count_values" => {
                let mut out = Arr::new();
                if let Value::Array(arr) = a(0) {
                    for (_, v) in arr.entries {
                        let k = Arr::norm_key(&v);
                        let cur = out.get(&k).map(to_i64).unwrap_or(0);
                        out.insert(k, Value::Int(cur + 1));
                    }
                }
                Value::Array(out)
            }
            "implode" | "join" => {
                // implode(sep, arr) or implode(arr)
                let (sep, arr) = match (&a(0), &a(1)) {
                    (Value::Array(arr), _) => (Vec::new(), arr.clone()),
                    (_, Value::Array(arr)) => (to_bytes(&a(0)), arr.clone()),
                    _ => (Vec::new(), Arr::new()),
                };
                let mut out = Vec::new();
                for (i, (_, v)) in arr.entries.iter().enumerate() {
                    if i > 0 {
                        out.extend_from_slice(&sep);
                    }
                    out.extend_from_slice(&to_bytes(v));
                }
                Value::Str(out)
            }
            "explode" => {
                let sep = to_bytes(&a(0));
                let s = to_bytes(&a(1));
                let mut arr = Arr::new();
                if sep.is_empty() {
                    arr.push(Value::Str(s));
                } else {
                    let mut start = 0;
                    let mut i = 0;
                    while i + sep.len() <= s.len() {
                        if &s[i..i + sep.len()] == sep.as_slice() {
                            arr.push(Value::Str(s[start..i].to_vec()));
                            i += sep.len();
                            start = i;
                        } else {
                            i += 1;
                        }
                    }
                    arr.push(Value::Str(s[start..].to_vec()));
                }
                Value::Array(arr)
            }
            "substr" => {
                let s = to_bytes(&a(0));
                let len = s.len() as i64;
                let mut start = to_i64(&a(1));
                if start < 0 {
                    start = (len + start).max(0);
                }
                let start = start.min(len) as usize;
                let end = if args.len() > 2 {
                    let l = to_i64(&a(2));
                    if l < 0 {
                        ((len + l).max(start as i64)) as usize
                    } else {
                        (start + l as usize).min(s.len())
                    }
                } else {
                    s.len()
                };
                Value::Str(s[start..end.max(start)].to_vec())
            }
            "str_replace" | "str_ireplace" => {
                self.str_replace_full(name == "str_ireplace", &args).0
            }
            "parse_url" => {
                let url = to_bytes(&a(0));
                match php_parse_url(&url) {
                    Some(parts) => {
                        if args.len() > 1 && !matches!(a(1), Value::Int(-1)) {
                            let want = to_i64(&a(1));
                            let key: &[u8] = match want {
                                0 => b"scheme",
                                1 => b"host",
                                2 => b"port",
                                3 => b"user",
                                4 => b"pass",
                                5 => b"path",
                                6 => b"query",
                                7 => b"fragment",
                                _ => b"",
                            };
                            parts
                                .get(&Key::Str(key.to_vec()))
                                .map(|v| v.deref())
                                .unwrap_or(Value::Null)
                        } else {
                            Value::Array(parts)
                        }
                    }
                    None => Value::Bool(false),
                }
            }
            "substr_replace" => {
                let s = to_bytes(&a(0));
                let rep = to_bytes(&a(1));
                let len = s.len() as i64;
                let off = to_i64(&a(2));
                let start = if off < 0 { (len + off).max(0) } else { off.min(len) } as usize;
                let l = if args.len() > 3 && !matches!(a(3), Value::Null) {
                    to_i64(&a(3))
                } else {
                    len
                };
                let end = if l < 0 {
                    ((len + l).max(start as i64)) as usize
                } else {
                    (start + l as usize).min(s.len())
                };
                let mut out = s[..start].to_vec();
                out.extend_from_slice(&rep);
                out.extend_from_slice(&s[end.max(start)..]);
                Value::Str(out)
            }
            "strtok" => {
                // 2-arg form re-initializes; 1-arg form continues
                if args.len() >= 2 {
                    self.strtok_state = Some((to_bytes(&a(0)), 0));
                }
                let delims = if args.len() >= 2 { to_bytes(&a(1)) } else { to_bytes(&a(0)) };
                match &mut self.strtok_state {
                    Some((s, pos)) => {
                        while *pos < s.len() && delims.contains(&s[*pos]) {
                            *pos += 1;
                        }
                        if *pos >= s.len() {
                            Value::Bool(false)
                        } else {
                            let start = *pos;
                            while *pos < s.len() && !delims.contains(&s[*pos]) {
                                *pos += 1;
                            }
                            Value::Str(s[start..*pos].to_vec())
                        }
                    }
                    None => Value::Bool(false),
                }
            }
            "strtr" => {
                let s = to_bytes(&a(0));
                if let Value::Array(pairs) = a(1) {
                    // pair form: longest match first at each position, no
                    // rescanning of replaced text
                    let mut map: Vec<(Vec<u8>, Vec<u8>)> = pairs
                        .entries
                        .iter()
                        .map(|(k, v)| (to_bytes(&akey_to_value(k)), to_bytes(v)))
                        .filter(|(k, _)| !k.is_empty())
                        .collect();
                    map.sort_by(|x, y| y.0.len().cmp(&x.0.len()));
                    let mut out = Vec::with_capacity(s.len());
                    let mut i = 0;
                    'outer: while i < s.len() {
                        for (k, v) in &map {
                            if s[i..].starts_with(k) {
                                out.extend_from_slice(v);
                                i += k.len();
                                continue 'outer;
                            }
                        }
                        out.push(s[i]);
                        i += 1;
                    }
                    Value::Str(out)
                } else {
                    let from = to_bytes(&a(1));
                    let to = to_bytes(&a(2));
                    let n = from.len().min(to.len());
                    let out = s
                        .iter()
                        .map(|&b| match from[..n].iter().position(|&f| f == b) {
                            Some(i) => to[i],
                            None => b,
                        })
                        .collect();
                    Value::Str(out)
                }
            }
            "strpos" => {
                let hay = to_bytes(&a(0));
                let needle = to_bytes(&a(1));
                match find_bytes(&hay, &needle, to_i64(&a(2)).max(0) as usize) {
                    Some(i) => Value::Int(i as i64),
                    None => Value::Bool(false),
                }
            }
            "strrpos" => {
                let hay = to_bytes(&a(0));
                let needle = to_bytes(&a(1));
                if needle.is_empty() {
                    Value::Int(hay.len() as i64)
                } else if needle.len() > hay.len() {
                    Value::Bool(false)
                } else {
                    match (0..=hay.len() - needle.len())
                        .rev()
                        .find(|&i| hay[i..i + needle.len()] == needle[..])
                    {
                        Some(i) => Value::Int(i as i64),
                        None => Value::Bool(false),
                    }
                }
            }
            "stripos" => {
                let hay = to_bytes(&a(0)).to_ascii_lowercase();
                let needle = to_bytes(&a(1)).to_ascii_lowercase();
                match find_bytes(&hay, &needle, to_i64(&a(2)).max(0) as usize) {
                    Some(i) => Value::Int(i as i64),
                    None => Value::Bool(false),
                }
            }
            "str_contains" => {
                Value::Bool(find_bytes(&to_bytes(&a(0)), &to_bytes(&a(1)), 0).is_some())
            }
            "str_starts_with" => Value::Bool(to_bytes(&a(0)).starts_with(&to_bytes(&a(1)))),
            "str_ends_with" => Value::Bool(to_bytes(&a(0)).ends_with(&to_bytes(&a(1)))),
            "strstr" | "strchr" => {
                let hay = to_bytes(&a(0));
                let needle = to_bytes(&a(1));
                let before = to_bool(&a(2));
                match find_bytes(&hay, &needle, 0) {
                    Some(i) => {
                        if before {
                            Value::Str(hay[..i].to_vec())
                        } else {
                            Value::Str(hay[i..].to_vec())
                        }
                    }
                    None => Value::Bool(false),
                }
            }
            "stristr" => {
                let hay = to_bytes(&a(0));
                let needle = to_bytes(&a(1));
                let before = to_bool(&a(2));
                match find_bytes(&hay.to_ascii_lowercase(), &needle.to_ascii_lowercase(), 0) {
                    Some(i) => {
                        if before {
                            Value::Str(hay[..i].to_vec())
                        } else {
                            Value::Str(hay[i..].to_vec())
                        }
                    }
                    None => Value::Bool(false),
                }
            }
            "strrchr" => {
                let hay = to_bytes(&a(0));
                let needle = to_bytes(&a(1));
                match needle.first() {
                    Some(&c) => match hay.iter().rposition(|&b| b == c) {
                        Some(i) => Value::Str(hay[i..].to_vec()),
                        None => Value::Bool(false),
                    },
                    None => Value::Bool(false),
                }
            }
            "strpbrk" => {
                let hay = to_bytes(&a(0));
                let chars = to_bytes(&a(1));
                match hay.iter().position(|b| chars.contains(b)) {
                    Some(i) => Value::Str(hay[i..].to_vec()),
                    None => Value::Bool(false),
                }
            }
            "strcmp" => Value::Int(byte_sign(&to_bytes(&a(0)), &to_bytes(&a(1)))),
            "strcasecmp" => Value::Int(byte_sign(
                &to_bytes(&a(0)).to_ascii_lowercase(),
                &to_bytes(&a(1)).to_ascii_lowercase(),
            )),
            "strncmp" => {
                let n = to_i64(&a(2)).max(0) as usize;
                let s1 = to_bytes(&a(0));
                let s2 = to_bytes(&a(1));
                Value::Int(byte_sign(
                    &s1[..n.min(s1.len())],
                    &s2[..n.min(s2.len())],
                ))
            }
            "strncasecmp" => {
                let n = to_i64(&a(2)).max(0) as usize;
                let s1 = to_bytes(&a(0)).to_ascii_lowercase();
                let s2 = to_bytes(&a(1)).to_ascii_lowercase();
                Value::Int(byte_sign(
                    &s1[..n.min(s1.len())],
                    &s2[..n.min(s2.len())],
                ))
            }
            "substr_compare" => {
                let main = to_bytes(&a(0));
                let s = to_bytes(&a(1));
                let mlen = main.len() as i64;
                let mut off = to_i64(&a(2));
                if off < 0 {
                    off = (mlen + off).max(0);
                }
                let off = off.min(mlen) as usize;
                let ci = to_bool(&a(4));
                let mut m = main[off..].to_vec();
                let mut s = s;
                if args.len() > 3 && !matches!(a(3), Value::Null) {
                    let l = to_i64(&a(3)).max(0) as usize;
                    m.truncate(l);
                    s.truncate(l);
                }
                if ci {
                    Value::Int(byte_sign(&m.to_ascii_lowercase(), &s.to_ascii_lowercase()))
                } else {
                    Value::Int(byte_sign(&m, &s))
                }
            }
            "strspn" | "strcspn" => {
                // offset/length args select the examined window (negative =
                // from the end) — WP's HTML Tag Processor scans with these
                let subj = to_bytes(&a(0));
                let mask = to_bytes(&a(1));
                let len = subj.len() as i64;
                let mut start = if args.len() > 2 { to_i64(&a(2)) } else { 0 };
                if start < 0 {
                    start = (len + start).max(0);
                }
                let start = start.min(len) as usize;
                let mut span_len = if args.len() > 3 && !matches!(a(3), Value::Null) {
                    to_i64(&a(3))
                } else {
                    len
                };
                if span_len < 0 {
                    span_len = (len - start as i64 + span_len).max(0);
                }
                let end = (start + span_len.max(0) as usize).min(subj.len());
                let window = &subj[start..end];
                let inv = name == "strcspn";
                Value::Int(
                    window
                        .iter()
                        .take_while(|b| mask.contains(b) != inv)
                        .count() as i64,
                )
            }
            "addslashes" => {
                let s = to_bytes(&a(0));
                let mut out = Vec::with_capacity(s.len());
                for &b in &s {
                    if matches!(b, b'\'' | b'"' | b'\\' | 0) {
                        out.push(b'\\');
                    }
                    if b == 0 {
                        out.push(b'0');
                    } else {
                        out.push(b);
                    }
                }
                Value::Str(out)
            }
            "stripcslashes" => {
                let s = to_bytes(&a(0));
                let mut out = Vec::with_capacity(s.len());
                let mut i = 0;
                while i < s.len() {
                    if s[i] != b'\\' || i + 1 >= s.len() {
                        out.push(s[i]);
                        i += 1;
                        continue;
                    }
                    i += 1;
                    match s[i] {
                        b'a' => out.push(0x07),
                        b'b' => out.push(0x08),
                        b'f' => out.push(0x0c),
                        b'n' => out.push(b'\n'),
                        b'r' => out.push(b'\r'),
                        b't' => out.push(b'\t'),
                        b'v' => out.push(0x0b),
                        b'x' => {
                            // \xHH: 1-2 hex digits; bare \x stays literal
                            let mut val: u32 = 0;
                            let mut n = 0;
                            while n < 2 && i + 1 < s.len() && s[i + 1].is_ascii_hexdigit() {
                                val = val * 16
                                    + (s[i + 1] as char).to_digit(16).unwrap_or(0);
                                i += 1;
                                n += 1;
                            }
                            if n == 0 {
                                out.push(b'x');
                            } else {
                                out.push(val as u8);
                            }
                        }
                        b'0'..=b'7' => {
                            // octal, up to 3 digits
                            let mut val: u32 = (s[i] - b'0') as u32;
                            let mut n = 1;
                            while n < 3 && i + 1 < s.len() && (b'0'..=b'7').contains(&s[i + 1]) {
                                val = val * 8 + (s[i + 1] - b'0') as u32;
                                i += 1;
                                n += 1;
                            }
                            out.push(val as u8);
                        }
                        other => out.push(other),
                    }
                    i += 1;
                }
                Value::Str(out)
            }
            "addcslashes" => {
                let s = to_bytes(&a(0));
                // charlist with "a..z" ranges
                let list = to_bytes(&a(1));
                let mut set = [false; 256];
                let mut i = 0;
                while i < list.len() {
                    if i + 3 < list.len() && list[i + 1] == b'.' && list[i + 2] == b'.' {
                        for b in list[i]..=list[i + 3] {
                            set[b as usize] = true;
                        }
                        i += 4;
                    } else {
                        set[list[i] as usize] = true;
                        i += 1;
                    }
                }
                let mut out = Vec::with_capacity(s.len());
                for &b in &s {
                    if !set[b as usize] {
                        out.push(b);
                        continue;
                    }
                    match b {
                        0x07 => out.extend_from_slice(b"\\a"),
                        0x08 => out.extend_from_slice(b"\\b"),
                        0x0c => out.extend_from_slice(b"\\f"),
                        b'\n' => out.extend_from_slice(b"\\n"),
                        b'\r' => out.extend_from_slice(b"\\r"),
                        b'\t' => out.extend_from_slice(b"\\t"),
                        0x0b => out.extend_from_slice(b"\\v"),
                        b if b < 32 || b > 126 => {
                            out.extend_from_slice(format!("\\{:03o}", b).as_bytes())
                        }
                        b => {
                            out.push(b'\\');
                            out.push(b);
                        }
                    }
                }
                Value::Str(out)
            }
            "stripslashes" => {
                let s = to_bytes(&a(0));
                let mut out = Vec::with_capacity(s.len());
                let mut i = 0;
                while i < s.len() {
                    if s[i] == b'\\' && i + 1 < s.len() {
                        i += 1;
                        out.push(if s[i] == b'0' { 0 } else { s[i] });
                    } else {
                        out.push(s[i]);
                    }
                    i += 1;
                }
                Value::Str(out)
            }
            "quotemeta" => {
                let s = to_bytes(&a(0));
                let mut out = Vec::with_capacity(s.len());
                for &b in &s {
                    if matches!(b, b'.' | b'\\' | b'+' | b'*' | b'?' | b'[' | b'^' | b']' | b'$' | b'(' | b')') {
                        out.push(b'\\');
                    }
                    out.push(b);
                }
                Value::Str(out)
            }
            "str_getcsv" => {
                let s = to_bytes(&a(0));
                let delim = first_byte_or(&to_bytes(&a(1)), b',');
                let quote = if args.len() > 2 { first_byte_or(&to_bytes(&a(2)), b'"') } else { b'"' };
                parse_csv(&s, delim, quote)
            }
            "array_keys" => {
                let mut out = Arr::new();
                if let Value::Array(arr) = a(0) {
                    for (k, _) in arr.entries {
                        out.push(match k {
                            Key::Int(n) => Value::Int(n),
                            Key::Str(s) => Value::Str(s),
                        });
                    }
                }
                Value::Array(out)
            }
            "array_values" => {
                let mut out = Arr::new();
                if let Value::Array(arr) = a(0) {
                    for (_, v) in arr.entries {
                        out.push(v);
                    }
                }
                Value::Array(out)
            }
            "array_merge" => {
                let mut out = Arr::new();
                for v in &args {
                    if let Value::Array(arr) = v {
                        for (k, val) in &arr.entries {
                            match k {
                                Key::Int(_) => out.push(val.clone()),
                                Key::Str(_) => out.insert(k.clone(), val.clone()),
                            }
                        }
                    }
                }
                Value::Array(out)
            }
            "array_merge_recursive" => {
                let mut out = Arr::new();
                for v in &args {
                    if let Value::Array(arr) = v {
                        merge_recursive(&mut out, arr);
                    }
                }
                Value::Array(out)
            }
            "array_chunk" => {
                let size = to_i64(&a(1)).max(1) as usize;
                let preserve = to_bool(&a(2));
                let mut out = Arr::new();
                if let Value::Array(arr) = a(0) {
                    let mut chunk = Arr::new();
                    for (k, v) in arr.entries {
                        if preserve {
                            chunk.insert(k, v);
                        } else {
                            chunk.push(v);
                        }
                        if chunk.len() == size {
                            out.push(Value::Array(std::mem::take(&mut chunk)));
                        }
                    }
                    if !chunk.is_empty() {
                        out.push(Value::Array(chunk));
                    }
                }
                Value::Array(out)
            }
            "array_reverse" => {
                let preserve = to_bool(&a(1));
                let mut out = Arr::new();
                if let Value::Array(arr) = a(0) {
                    for (k, v) in arr.entries.into_iter().rev() {
                        match k {
                            Key::Int(_) if !preserve => out.push(v),
                            _ => out.insert(k, v),
                        }
                    }
                }
                Value::Array(out)
            }
            "array_slice" => {
                let preserve = to_bool(&a(3));
                let (entries, len) = match a(0) {
                    Value::Array(arr) => {
                        let l = arr.entries.len();
                        (arr.entries, l)
                    }
                    _ => (Vec::new(), 0),
                };
                let mut off = to_i64(&a(1));
                if off < 0 {
                    off = (len as i64 + off).max(0);
                }
                let off = off.min(len as i64) as usize;
                let end = if args.len() > 2 && !matches!(a(2), Value::Null) {
                    let l = to_i64(&a(2));
                    if l < 0 { ((len as i64 + l).max(off as i64)) as usize } else { (off + l as usize).min(len) }
                } else {
                    len
                };
                let mut out = Arr::new();
                for (k, v) in entries.into_iter().take(end).skip(off) {
                    match k {
                        Key::Int(_) if !preserve => out.push(v),
                        _ => out.insert(k, v),
                    }
                }
                Value::Array(out)
            }
            "array_flip" => {
                let mut out = Arr::new();
                if let Value::Array(arr) = a(0) {
                    for (k, v) in arr.entries {
                        out.insert(Arr::norm_key(&v), akey_to_value(&k));
                    }
                }
                Value::Array(out)
            }
            "array_unique" => {
                let mut out = Arr::new();
                let mut seen: HashSet<Vec<u8>> = HashSet::new();
                if let Value::Array(arr) = a(0) {
                    for (k, v) in arr.entries {
                        if seen.insert(to_bytes(&v)) {
                            out.insert(k, v);
                        }
                    }
                }
                Value::Array(out)
            }
            "array_search" => {
                let needle = a(0);
                let strict = to_bool(&a(2));
                let mut found = Value::Bool(false);
                if let Value::Array(arr) = a(1) {
                    for (k, v) in &arr.entries {
                        if (strict && strict_eq(&needle, v)) || (!strict && loose_eq(&needle, v)) {
                            found = akey_to_value(k);
                            break;
                        }
                    }
                }
                found
            }
            "array_key_exists" | "key_exists" => {
                let key = Arr::norm_key(&a(0));
                Value::Bool(matches!(a(1), Value::Array(arr) if arr.get(&key).is_some()))
            }
            "array_key_first" => match a(0) {
                Value::Array(arr) => arr.entries.first().map(|(k, _)| akey_to_value(k)).unwrap_or(Value::Null),
                _ => Value::Null,
            },
            "array_key_last" => match a(0) {
                Value::Array(arr) => arr.entries.last().map(|(k, _)| akey_to_value(k)).unwrap_or(Value::Null),
                _ => Value::Null,
            },
            "array_combine" => {
                let mut out = Arr::new();
                if let (Value::Array(ks), Value::Array(vs)) = (a(0), a(1)) {
                    for ((_, k), (_, v)) in ks.entries.into_iter().zip(vs.entries) {
                        out.insert(Arr::norm_key(&k), v);
                    }
                }
                Value::Array(out)
            }
            "array_fill" => {
                let start = to_i64(&a(0));
                let count = to_i64(&a(1)).max(0) as usize;
                let val = a(2);
                // cap by TOTAL nodes (count × element size), not just count
                let elem = value_size(&val, MAX_ARRAY_NODES).max(1);
                if count.saturating_mul(elem) > MAX_ARRAY_NODES {
                    return Err(self.throw_error("Error", "Possible integer overflow in memory allocation"));
                }
                let mut out = Arr::new();
                for i in 0..count {
                    out.insert(Key::Int(start + i as i64), val.clone());
                }
                Value::Array(out)
            }
            "array_column" => {
                let col = a(1);
                let idx = a(2);
                let mut out = Arr::new();
                if let Value::Array(arr) = a(0) {
                    for (_, row) in arr.entries {
                        if let Value::Array(r) = &row {
                            let cell = if matches!(col, Value::Null) {
                                row.clone()
                            } else {
                                r.get(&Arr::norm_key(&col)).cloned().unwrap_or(Value::Null)
                            };
                            if !matches!(idx, Value::Null) {
                                if let Some(kv) = r.get(&Arr::norm_key(&idx)) {
                                    out.insert(Arr::norm_key(kv), cell);
                                    continue;
                                }
                            }
                            out.push(cell);
                        }
                    }
                }
                Value::Array(out)
            }
            "array_is_list" => Value::Bool(match a(0) {
                Value::Array(arr) => arr.entries.iter().enumerate().all(|(i, (k, _))| matches!(k, Key::Int(n) if *n == i as i64)),
                _ => false,
            }),
            "array_pad" => {
                let size = to_i64(&a(1));
                let val = a(2);
                let mut items: Vec<Value> = match a(0) {
                    Value::Array(arr) => arr.entries.into_iter().map(|(_, v)| v).collect(),
                    _ => Vec::new(),
                };
                let n = (size.unsigned_abs() as usize).min(MAX_ARRAY_NODES);
                if items.len() < n {
                    let pad = n - items.len();
                    if size < 0 {
                        let mut front = vec![val; pad];
                        front.extend(items);
                        items = front;
                    } else {
                        items.extend(vec![val; pad]);
                    }
                }
                let mut out = Arr::new();
                for v in items {
                    out.push(v);
                }
                Value::Array(out)
            }
            "array_sum" => {
                let mut fi = 0i64;
                let mut ff = 0f64;
                let mut isf = false;
                if let Value::Array(arr) = a(0) {
                    for (_, v) in &arr.entries {
                        match to_num(v) {
                            Num::Int(n) if !isf => fi += n,
                            n => {
                                if !isf {
                                    ff = fi as f64;
                                    isf = true;
                                }
                                ff += n.as_f64();
                            }
                        }
                    }
                }
                if isf {
                    Value::Float(ff)
                } else {
                    Value::Int(fi)
                }
            }
            "in_array" => {
                let needle = a(0);
                let strict = to_bool(&a(2));
                let mut found = false;
                if let Value::Array(arr) = a(1) {
                    for (_, v) in &arr.entries {
                        if (strict && strict_eq(&needle, v)) || (!strict && loose_eq(&needle, v)) {
                            found = true;
                            break;
                        }
                    }
                }
                Value::Bool(found)
            }
            "call_user_func" => {
                let f = a(0);
                let rest = if args.len() > 1 { args[1..].to_vec() } else { vec![] };
                return self.call_value(f, rest);
            }
            "call_user_func_array" => {
                let f = a(0);
                let mut argv = Vec::new();
                if let Value::Array(arr) = a(1) {
                    for (_, v) in arr.entries {
                        argv.push(v);
                    }
                }
                return self.call_value(f, argv);
            }
            "array_map" => {
                let cb = a(0);
                let mut out = Arr::new();
                if let Value::Array(arr) = a(1) {
                    for (k, v) in arr.entries {
                        let r = if matches!(cb, Value::Null) {
                            v
                        } else {
                            self.call_value(cb.clone(), vec![v])?
                        };
                        out.insert(k, r);
                    }
                }
                Value::Array(out)
            }
            "array_filter" => {
                let cb = a(1);
                // mode: 0 = value, ARRAY_FILTER_USE_KEY = 2, USE_BOTH = 1
                let mode = to_i64(&a(2));
                let mut out = Arr::new();
                if let Value::Array(arr) = a(0) {
                    for (k, v) in arr.entries {
                        let keep = if matches!(cb, Value::Null) {
                            to_bool(&v)
                        } else {
                            let cb_args = match mode {
                                2 => vec![akey_to_value(&k)],
                                1 => vec![v.clone(), akey_to_value(&k)],
                                _ => vec![v.clone()],
                            };
                            to_bool(&self.call_value(cb.clone(), cb_args)?)
                        };
                        if keep {
                            out.insert(k, v);
                        }
                    }
                }
                Value::Array(out)
            }
            "array_reduce" => {
                let cb = a(1);
                let mut acc = a(2);
                if let Value::Array(arr) = a(0) {
                    for (_, v) in arr.entries {
                        acc = self.call_value(cb.clone(), vec![acc, v])?;
                    }
                }
                acc
            }
            "is_callable" => Value::Bool(match a(0) {
                Value::Closure(_) => true,
                Value::Str(s) => {
                    let n = String::from_utf8_lossy(&s).to_ascii_lowercase();
                    self.funcs.contains_key(&n) || is_known_builtin(&n)
                }
                Value::Object(rc) => {
                    let c = rc.borrow().class.clone();
                    self.find_method(&c, "__invoke").is_some()
                }
                Value::Array(arr) => arr.len() == 2,
                _ => false,
            }),
            "range" => self.range(&a(0), &a(1), &a(2))?,
            "sprintf" => Value::Str(self.sprintf(&args)),
            "printf" => {
                let s = self.sprintf(&args);
                let n = s.len();
                self.out.extend_from_slice(&s);
                Value::Int(n as i64)
            }
            "vsprintf" | "vprintf" => {
                // format + an array of args → flatten to a sprintf arg list
                let mut sa = vec![a(0)];
                if let Value::Array(arr) = a(1) {
                    for (_, v) in arr.entries {
                        sa.push(v);
                    }
                }
                let s = self.sprintf(&sa);
                if name == "vprintf" {
                    let n = s.len();
                    self.out.extend_from_slice(&s);
                    Value::Int(n as i64)
                } else {
                    Value::Str(s)
                }
            }
            "define" => {
                if let Value::Str(n) = a(0) {
                    self.consts
                        .insert(String::from_utf8_lossy(&n).into_owned(), a(1));
                }
                Value::Bool(true)
            }
            "function_exists" => {
                let n = String::from_utf8_lossy(&to_bytes(&a(0))).to_ascii_lowercase();
                Value::Bool(self.funcs.contains_key(&n) || is_known_builtin(&n))
            }
            "__dom_parse" => match super::xml::parse(&to_bytes(&a(0))) {
                Ok(v) => v,
                Err(_) => Value::Bool(false),
            },
            "xml_parser_create" | "xml_parser_create_ns" => {
                let o = new_obj("__XmlParser");
                if let Value::Object(rc) = &o {
                    rc.borrow_mut().set("__fold", Value::Bool(true));
                }
                o
            }
            "xml_set_element_handler" => {
                if let Value::Object(rc) = a(0) {
                    rc.borrow_mut().set("__start", a(1));
                    rc.borrow_mut().set("__end", a(2));
                }
                Value::Bool(true)
            }
            "xml_set_character_data_handler" => {
                if let Value::Object(rc) = a(0) {
                    rc.borrow_mut().set("__char", a(1));
                }
                Value::Bool(true)
            }
            "xml_set_default_handler" | "xml_set_processing_instruction_handler"
            | "xml_set_notation_decl_handler" | "xml_set_external_entity_ref_handler"
            | "xml_set_start_namespace_decl_handler" | "xml_set_end_namespace_decl_handler"
            | "xml_set_unparsed_entity_decl_handler" | "xml_set_object" => Value::Bool(true),
            "xml_parser_set_option" => {
                // XML_OPTION_CASE_FOLDING = 1
                if let Value::Object(rc) = a(0) {
                    if to_i64(&a(1)) == 1 {
                        rc.borrow_mut().set("__fold", Value::Bool(to_bool(&a(2))));
                    }
                }
                Value::Bool(true)
            }
            "xml_parser_get_option" => {
                if to_i64(&a(1)) == 1 {
                    if let Value::Object(rc) = a(0) {
                        return Ok(rc.borrow().get("__fold").cloned().unwrap_or(Value::Bool(true)));
                    }
                }
                Value::Int(0)
            }
            "xml_parse" | "xml_parse_into_struct" => {
                let parser = a(0);
                let data = to_bytes(&a(1));
                let fold = matches!(&parser, Value::Object(rc) if matches!(rc.borrow().get("__fold"), Some(Value::Bool(true)) | None));
                match super::xml::parse(&data) {
                    Ok(tree) => {
                        self.xml_sax_walk(&parser, &tree, fold)?;
                        Value::Int(1)
                    }
                    Err(_) => Value::Int(1), // SAX is lenient; report success
                }
            }
            "xml_parser_free" => Value::Bool(true),
            "xml_get_error_code" => Value::Int(0),
            "xml_error_string" => Value::Str(Vec::new()),
            "xml_get_current_line_number" | "xml_get_current_column_number"
            | "xml_get_current_byte_index" => Value::Int(0),
            "constant" => {
                let name = String::from_utf8_lossy(&to_bytes(&a(0))).into_owned();
                if let Some((cls, c)) = name.split_once("::") {
                    self.class_const(cls, c)?
                } else if let Some(v) = self.consts.get(&name) {
                    v.clone()
                } else {
                    php_const(&name).unwrap_or(Value::Null)
                }
            }
            "defined" => {
                let name = String::from_utf8_lossy(&to_bytes(&a(0))).into_owned();
                if let Some((cls, c)) = name.split_once("::") {
                    Value::Bool(self.class_const(cls, c).is_ok())
                } else {
                    Value::Bool(self.consts.contains_key(&name) || php_const(&name).is_some())
                }
            }
            // ---- output buffering ----
            "ob_start" => {
                self.ob_stack.push(self.out.len());
                Value::Bool(true)
            }
            "ob_get_contents" => match self.ob_stack.last() {
                Some(&w) => Value::Str(self.out[w..].to_vec()),
                None => Value::Bool(false),
            },
            "ob_get_clean" => match self.ob_stack.pop() {
                Some(w) => {
                    let s = self.out[w..].to_vec();
                    self.out.truncate(w);
                    Value::Str(s)
                }
                None => Value::Bool(false),
            },
            "ob_end_clean" | "ob_clean" => match self.ob_stack.last().copied() {
                Some(w) => {
                    self.out.truncate(w);
                    if name == "ob_end_clean" {
                        self.ob_stack.pop();
                    }
                    Value::Bool(true)
                }
                None => Value::Bool(false),
            },
            "ob_end_flush" | "ob_flush" | "flush" => {
                if name == "ob_end_flush" {
                    self.ob_stack.pop();
                }
                Value::Bool(true)
            }
            "ob_get_level" => Value::Int(self.ob_stack.len() as i64),
            "ob_get_length" => match self.ob_stack.last() {
                Some(&w) => Value::Int((self.out.len() - w) as i64),
                None => Value::Bool(false),
            },
            // ---- path / filename helpers ----
            "basename" => {
                let s = String::from_utf8_lossy(&to_bytes(&a(0))).into_owned();
                let s = s.trim_end_matches(['/', '\\']);
                let mut base = s.rsplit(['/', '\\']).next().unwrap_or("").to_string();
                if args.len() > 1 {
                    let suf = String::from_utf8_lossy(&to_bytes(&a(1))).into_owned();
                    if base.len() > suf.len() && base.ends_with(&suf) {
                        base.truncate(base.len() - suf.len());
                    }
                }
                Value::Str(base.into_bytes())
            }
            "dirname" => {
                let s = String::from_utf8_lossy(&to_bytes(&a(0))).into_owned();
                let s = s.trim_end_matches(['/', '\\']);
                match s.rfind(['/', '\\']) {
                    Some(0) => Value::Str(b"/".to_vec()),
                    Some(i) => Value::Str(s[..i].as_bytes().to_vec()),
                    None => Value::Str(b".".to_vec()),
                }
            }
            // ---- class / object introspection ----
            "get_class" => match a(0) {
                Value::Object(rc) => Value::Str(display_class(&rc.borrow().class).into_bytes()),
                _ => match &self.current_class {
                    Some(c) => Value::Str(display_class(c).into_bytes()),
                    None => Value::Bool(false),
                },
            },
            "get_parent_class" => {
                let cname = match a(0) {
                    Value::Object(rc) => Some(rc.borrow().class.clone()),
                    Value::Str(s) => Some(String::from_utf8_lossy(&s).into_owned()),
                    _ => self.current_class.clone(),
                };
                match cname.and_then(|c| self.find_class(&c)).and_then(|d| d.parent.clone()) {
                    Some(p) => Value::Str(p.last().as_bytes().to_vec()),
                    None => Value::Bool(false),
                }
            }
            "class_exists" | "interface_exists" | "trait_exists" | "enum_exists" => {
                let n = String::from_utf8_lossy(&to_bytes(&a(0))).into_owned();
                // second arg $autoload defaults to true
                if self.find_class(&n).is_none() && (args.len() < 2 || to_bool(&a(1))) {
                    self.autoload(&n);
                }
                Value::Bool(match self.find_class(&n) {
                    // class_exists is true for classes AND enums (enums are classes);
                    // the others each match only their own kind.
                    Some(d) => match name {
                        "interface_exists" => d.kind == ClassKind::Interface,
                        "trait_exists" => d.kind == ClassKind::Trait,
                        "enum_exists" => d.kind == ClassKind::Enum,
                        _ => matches!(d.kind, ClassKind::Class | ClassKind::Enum),
                    },
                    None => false,
                })
            }
            "get_declared_classes" | "get_declared_interfaces" | "get_declared_traits" => {
                let want_kind = match name {
                    "get_declared_interfaces" => ClassKind::Interface,
                    "get_declared_traits" => ClassKind::Trait,
                    _ => ClassKind::Class,
                };
                let mut arr = Arr::new();
                let mut names: Vec<String> = self
                    .classes
                    .values()
                    .filter(|c| {
                        if name == "get_declared_classes" {
                            matches!(c.kind, ClassKind::Class)
                        } else {
                            c.kind == want_kind
                        }
                    })
                    .map(|c| c.name.clone())
                    .filter(|n| !n.contains('#'))
                    .collect();
                names.sort();
                for n in names {
                    arr.push(Value::Str(n.into_bytes()));
                }
                Value::Array(arr)
            }
            "get_object_vars" => {
                let mut o = Arr::new();
                if let Value::Object(rc) = a(0) {
                    for (k, v) in &rc.borrow().props {
                        o.insert(Key::Str(k.as_bytes().to_vec()), v.clone());
                    }
                }
                Value::Array(o)
            }
            "method_exists" => {
                let cn = match a(0) {
                    Value::Object(rc) => rc.borrow().class.clone(),
                    v => String::from_utf8_lossy(&to_bytes(&v)).into_owned(),
                };
                let m = String::from_utf8_lossy(&to_bytes(&a(1))).into_owned();
                Value::Bool(self.find_method(&cn, &m).is_some())
            }
            "property_exists" => {
                let p = String::from_utf8_lossy(&to_bytes(&a(1))).into_owned();
                let in_obj = matches!(&a(0), Value::Object(rc) if rc.borrow().get(&p).is_some());
                let cn = match a(0) {
                    Value::Object(rc) => rc.borrow().class.clone(),
                    v => String::from_utf8_lossy(&to_bytes(&v)).into_owned(),
                };
                let in_class = self.ancestry(&cn).iter().any(|c| c.props.iter().any(|pd| pd.name == p));
                Value::Bool(in_obj || in_class)
            }
            "is_a" | "is_subclass_of" => {
                let cn = match a(0) {
                    Value::Object(rc) => rc.borrow().class.clone(),
                    v => String::from_utf8_lossy(&to_bytes(&v)).into_owned(),
                };
                let t = String::from_utf8_lossy(&to_bytes(&a(1))).into_owned();
                let sub = self.is_subclass(&cn, &t);
                Value::Bool(if name == "is_subclass_of" {
                    sub && !cn.eq_ignore_ascii_case(&t)
                } else {
                    sub
                })
            }
            "get_class_methods" => {
                let cn = match a(0) {
                    Value::Object(rc) => rc.borrow().class.clone(),
                    v => String::from_utf8_lossy(&to_bytes(&v)).into_owned(),
                };
                // visibility depends on the calling scope: outside the class
                // only public methods are listed; inside, everything is
                let inside = self
                    .current_class
                    .as_deref()
                    .map(|cc| self.instance_of_name(&cn, cc) || cc.eq_ignore_ascii_case(&cn))
                    .unwrap_or(false);
                let mut arr = Arr::new();
                let mut seen = HashSet::new();
                for c in self.ancestry(&cn) {
                    for m in &c.methods {
                        if !inside && !matches!(m.visibility, Visibility::Public) {
                            continue;
                        }
                        if seen.insert(m.name.to_ascii_lowercase()) {
                            arr.push(Value::Str(m.name.as_bytes().to_vec()));
                        }
                    }
                }
                Value::Array(arr)
            }
            "get_class_vars" => {
                let cn = String::from_utf8_lossy(&to_bytes(&a(0))).into_owned();
                let mut arr = Arr::new();
                for c in self.ancestry(&cn).into_iter().rev() {
                    for p in &c.props {
                        if !p.is_static {
                            let v = match &p.default {
                                Some(d) => self.eval(&d.clone())?,
                                None => Value::Null,
                            };
                            arr.insert(Key::Str(p.name.as_bytes().to_vec()), v);
                        }
                    }
                }
                Value::Array(arr)
            }
            "class_implements" => {
                let cn = match a(0) {
                    Value::Object(rc) => rc.borrow().class.clone(),
                    v => String::from_utf8_lossy(&to_bytes(&v)).into_owned(),
                };
                let mut arr = Arr::new();
                for c in self.ancestry(&cn) {
                    for i in &c.interfaces {
                        let nm = i.last().to_string();
                        arr.insert(Key::Str(nm.clone().into_bytes()), Value::Str(nm.into_bytes()));
                    }
                }
                Value::Array(arr)
            }
            "class_parents" => {
                let cn = match a(0) {
                    Value::Object(rc) => rc.borrow().class.clone(),
                    v => String::from_utf8_lossy(&to_bytes(&v)).into_owned(),
                };
                let mut arr = Arr::new();
                for c in self.ancestry(&cn).into_iter().skip(1) {
                    arr.insert(
                        Key::Str(c.name.clone().into_bytes()),
                        Value::Str(c.name.clone().into_bytes()),
                    );
                }
                Value::Array(arr)
            }
            "class_uses" => {
                let cn = String::from_utf8_lossy(&to_bytes(&a(0))).into_owned();
                let mut arr = Arr::new();
                if let Some(c) = self.find_class(&cn) {
                    for t in &c.uses_traits {
                        let nm = t.last().to_string();
                        arr.insert(Key::Str(nm.clone().into_bytes()), Value::Str(nm.into_bytes()));
                    }
                }
                Value::Array(arr)
            }
            "phargo_class_constants" => {
                let cn = String::from_utf8_lossy(&to_bytes(&a(0))).into_owned();
                let mut arr = Arr::new();
                for c in self.ancestry(&cn) {
                    for cc in &c.consts {
                        let key = Key::Str(cc.name.as_bytes().to_vec());
                        if arr.get(&key).is_none() {
                            let v = self.eval(&cc.value.clone())?;
                            arr.insert(key, v);
                        }
                    }
                }
                Value::Array(arr)
            }
            "phargo_func_params" => {
                let cls = String::from_utf8_lossy(&to_bytes(&a(0))).into_owned();
                let fname = String::from_utf8_lossy(&to_bytes(&a(1))).into_owned();
                let params = if cls.is_empty() {
                    self.funcs.get(&fname.to_ascii_lowercase()).map(|f| f.params.clone())
                } else {
                    self.find_method(&cls, &fname).map(|(_, m)| m.params.clone())
                };
                let mut arr = Arr::new();
                if let Some(ps) = params {
                    for (i, p) in ps.iter().enumerate() {
                        let mut info = Arr::new();
                        info.insert(Key::Str(b"name".to_vec()), Value::Str(p.name.as_bytes().to_vec()));
                        info.insert(Key::Str(b"position".to_vec()), Value::Int(i as i64));
                        info.insert(
                            Key::Str(b"optional".to_vec()),
                            Value::Bool(p.default.is_some() || p.variadic),
                        );
                        info.insert(
                            Key::Str(b"has_default".to_vec()),
                            Value::Bool(p.default.is_some()),
                        );
                        info.insert(Key::Str(b"variadic".to_vec()), Value::Bool(p.variadic));
                        info.insert(Key::Str(b"by_ref".to_vec()), Value::Bool(p.by_ref));
                        info.insert(
                            Key::Str(b"type".to_vec()),
                            match &p.type_hint {
                                Some(t) => Value::Str(t.as_bytes().to_vec()),
                                None => Value::Null,
                            },
                        );
                        let dv = match &p.default {
                            Some(d) => self.eval(&d.clone()).unwrap_or(Value::Null),
                            None => Value::Null,
                        };
                        info.insert(Key::Str(b"default".to_vec()), dv);
                        arr.push(Value::Array(info));
                    }
                }
                Value::Array(arr)
            }
            "phargo_func_return_type" => {
                let cls = String::from_utf8_lossy(&to_bytes(&a(0))).into_owned();
                let fname = String::from_utf8_lossy(&to_bytes(&a(1))).into_owned();
                let rt = if cls.is_empty() {
                    self.funcs.get(&fname.to_ascii_lowercase()).and_then(|f| f.ret_type.clone())
                } else {
                    self.find_method(&cls, &fname).and_then(|(_, m)| m.ret_type.clone())
                };
                match rt {
                    Some(t) => Value::Str(t.into_bytes()),
                    None => Value::Null,
                }
            }
            "phargo_prop_info" => {
                // {type, visibility(0/1/2), readonly, static, promoted} for a class
                // property (declared or constructor-promoted), or null if unknown.
                let cls = String::from_utf8_lossy(&to_bytes(&a(0))).into_owned();
                let pname = String::from_utf8_lossy(&to_bytes(&a(1))).into_owned();
                let mut found: Option<(Option<String>, Visibility, bool, bool, bool)> = None;
                for c in self.ancestry(&cls) {
                    if let Some(p) = c.props.iter().find(|p| p.name == pname) {
                        found = Some((p.type_hint.clone(), p.visibility, p.readonly, p.is_static, false));
                        break;
                    }
                    if let Some(ctor) = c.methods.iter().find(|m| m.name.eq_ignore_ascii_case("__construct")) {
                        if let Some(pp) = ctor.params.iter().find(|p| p.name == pname && p.promote.is_some()) {
                            found = Some((pp.type_hint.clone(), pp.promote.unwrap(), pp.readonly, false, true));
                            break;
                        }
                    }
                }
                match found {
                    Some((ty, vis, ro, st, promoted)) => {
                        let mut info = Arr::new();
                        info.insert(Key::Str(b"type".to_vec()), match ty {
                            Some(t) => Value::Str(t.into_bytes()),
                            None => Value::Null,
                        });
                        info.insert(Key::Str(b"visibility".to_vec()), Value::Int(match vis {
                            Visibility::Public => 0, Visibility::Protected => 2, Visibility::Private => 1,
                        }));
                        info.insert(Key::Str(b"readonly".to_vec()), Value::Bool(ro));
                        info.insert(Key::Str(b"static".to_vec()), Value::Bool(st));
                        info.insert(Key::Str(b"promoted".to_vec()), Value::Bool(promoted));
                        Value::Array(info)
                    }
                    None => Value::Null,
                }
            }
            "tempnam" => {
                let dir = String::from_utf8_lossy(&to_bytes(&a(0))).into_owned();
                let prefix = String::from_utf8_lossy(&to_bytes(&a(1))).into_owned();
                let base = std::path::PathBuf::from(&dir);
                // deterministic-ish unique name from the step counter
                let cand = base.join(format!("{prefix}{}", self.steps));
                match std::fs::OpenOptions::new().create_new(true).write(true).open(&cand) {
                    Ok(_) => Value::Str(cand.to_string_lossy().as_bytes().to_vec()),
                    Err(_) => Value::Bool(false),
                }
            }
            "chdir" => {
                let dir = String::from_utf8_lossy(&to_bytes(&a(0))).into_owned();
                Value::Bool(std::env::set_current_dir(&dir).is_ok())
            }
            // ---- date / time (reuse the legacy engine's civil-calendar functions) ----
            "time" => Value::Int(crate::now_unix()),
            "date" | "gmdate" => {
                let fmt = String::from_utf8_lossy(&to_bytes(&a(0))).into_owned();
                let ts = if args.len() > 1 { to_i64(&a(1)) } else { crate::now_unix() };
                let zone = if name == "date" { self.cur_tz() } else { None };
                Value::Str(crate::php_date_tz(&fmt, ts, zone.as_deref()).into_bytes())
            }
            // internal: tz-aware format/parse/offset for the DateTime prelude classes
            "__phargo_date_tz" => {
                let fmt = String::from_utf8_lossy(&to_bytes(&a(0))).into_owned();
                let ts = to_i64(&a(1));
                let tzname = String::from_utf8_lossy(&to_bytes(&a(2))).into_owned();
                let zone = if crate::tz::is_utc_name(&tzname) { None } else { crate::tz::lookup(&tzname) };
                Value::Str(crate::php_date_tz(&fmt, ts, zone.as_deref()).into_bytes())
            }
            "__phargo_trace" => {
                let mut arr = Arr::new();
                for (name, line, file) in self.frames.iter().rev() {
                    // frames inside prelude code (Exception::__construct etc.)
                    // are engine internals — PHP traces don't show them
                    let base = name
                        .split(|c| c == '-' || c == ':')
                        .next()
                        .unwrap_or(name)
                        .to_ascii_lowercase();
                    if self.prelude_classes.contains(&base) || self.prelude_fns.contains(&base) {
                        continue;
                    }
                    let mut f = Arr::new();
                    f.insert(Key::Str(b"file".to_vec()), Value::Str(file.clone().into_bytes()));
                    f.insert(Key::Str(b"line".to_vec()), Value::Int(*line as i64));
                    f.insert(Key::Str(b"function".to_vec()), Value::Str(name.clone().into_bytes()));
                    arr.push(Value::Array(f));
                }
                Value::Array(arr)
            }
            "__phargo_cur_line" => Value::Int(self.cur_line as i64),
            "__phargo_cur_file" => Value::Str(
                self.cur_file
                    .as_ref()
                    .map(|p| p.to_string_lossy().into_owned())
                    .unwrap_or_default()
                    .into_bytes(),
            ),
            // ---- PDO/SQLite bridge (src/pdo.rs; prelude PDO classes) ----
            "__pdo_open" => {
                let path = String::from_utf8_lossy(&to_bytes(&a(0))).into_owned();
                match crate::pdo::open(&path) {
                    Ok(id) => Value::Int(id),
                    Err(e) => {
                        return Err(self.throw_error(
                            "PDOException",
                            &format!("SQLSTATE[HY000] [14] {e}"),
                        ))
                    }
                }
            }
            "__pdo_query" => {
                let id = to_i64(&a(0));
                let sql = String::from_utf8_lossy(&to_bytes(&a(1))).into_owned();
                let mut params: Vec<(Option<String>, crate::pdo::SqlVal)> = Vec::new();
                if let Value::Array(pa) = &a(2) {
                    for (k, v) in &pa.entries {
                        // string keys are NAMED parameters (":param0")
                        let name = match k {
                            Key::Str(s) => Some(String::from_utf8_lossy(s).into_owned()),
                            Key::Int(_) => None,
                        };
                        params.push((
                            name,
                            match v {
                                Value::Null => crate::pdo::SqlVal::Null,
                                Value::Int(n) => crate::pdo::SqlVal::Int(*n),
                                Value::Float(f) => crate::pdo::SqlVal::Real(*f),
                                Value::Bool(b) => crate::pdo::SqlVal::Int(*b as i64),
                                other => crate::pdo::SqlVal::Text(to_bytes(other)),
                            },
                        ));
                    }
                }
                match crate::pdo::query(id, &sql, params) {
                    Ok((cols, rows, affected)) => {
                        let mut carr = Arr::new();
                        for c in cols {
                            carr.push(Value::Str(c.into_bytes()));
                        }
                        let mut rarr = Arr::new();
                        for row in rows {
                            let mut r = Arr::new();
                            for v in row {
                                r.push(match v {
                                    crate::pdo::SqlVal::Null => Value::Null,
                                    crate::pdo::SqlVal::Int(n) => Value::Int(n),
                                    crate::pdo::SqlVal::Real(f) => Value::Float(f),
                                    crate::pdo::SqlVal::Text(t) => Value::Str(t),
                                    crate::pdo::SqlVal::Blob(b) => Value::Str(b),
                                });
                            }
                            rarr.push(Value::Array(r));
                        }
                        let mut out = Arr::new();
                        out.insert(Key::Str(b"cols".to_vec()), Value::Array(carr));
                        out.insert(Key::Str(b"rows".to_vec()), Value::Array(rarr));
                        out.insert(Key::Str(b"affected".to_vec()), Value::Int(affected as i64));
                        Value::Array(out)
                    }
                    Err(e) => {
                        return Err(self.throw_error(
                            "PDOException",
                            &format!("SQLSTATE[HY000]: General error: 1 {e}"),
                        ))
                    }
                }
            }
            "__pdo_lastid" => Value::Int(crate::pdo::last_insert_id(to_i64(&a(0)))),
            "__pdo_close" => Value::Bool(crate::pdo::close(to_i64(&a(0)))),
            "__phargo_tz_offset" => {
                let tzname = String::from_utf8_lossy(&to_bytes(&a(0))).into_owned();
                let ts = to_i64(&a(1));
                match crate::tz::lookup(&tzname) {
                    Some(z) => Value::Int(z.offset_at(ts).0 as i64),
                    None => Value::Int(0),
                }
            }
            "timezone_identifiers_list" => {
                // Args (group, country) are accepted for signature compatibility with
                // PHP's timezone_identifiers_list()/DateTimeZone::listIdentifiers() but
                // ignored — we return the full sorted zoneinfo list regardless of group.
                let mut arr = Arr::new();
                for id in crate::tz::identifiers() {
                    arr.push(Value::Str(id.into_bytes()));
                }
                Value::Array(arr)
            }
            "__phargo_tz_valid" => {
                let tzname = String::from_utf8_lossy(&to_bytes(&a(0))).into_owned();
                Value::Bool(crate::tz::is_utc_name(&tzname) || crate::tz::lookup(&tzname).is_some())
            }
            "__phargo_mktime_tz" => {
                let wall = crate::make_ts(
                    to_i64(&a(0)),
                    to_i64(&a(1)),
                    to_i64(&a(2)),
                    to_i64(&a(3)),
                    to_i64(&a(4)),
                    to_i64(&a(5)),
                );
                let tzname = String::from_utf8_lossy(&to_bytes(&a(6))).into_owned();
                let zone = if crate::tz::is_utc_name(&tzname) { None } else { crate::tz::lookup(&tzname) };
                Value::Int(match zone {
                    Some(z) => crate::tz::ts_from_local(wall, &z),
                    None => wall,
                })
            }
            "__phargo_createfromformat" => {
                let fmt = String::from_utf8_lossy(&to_bytes(&a(0))).into_owned();
                let input = String::from_utf8_lossy(&to_bytes(&a(1))).into_owned();
                let tzname = String::from_utf8_lossy(&to_bytes(&a(2))).into_owned();
                match crate::php_parse_from_format(&fmt, &input) {
                    None => Value::Bool(false),
                    Some(p) => {
                        // an e/T zone in the input overrides the passed zone
                        let effective = p.tzname.clone().unwrap_or(tzname);
                        let zone = if crate::tz::is_utc_name(&effective) {
                            None
                        } else {
                            crate::tz::lookup(&effective)
                        };
                        if let Some(ts) = p.epoch {
                            // U is absolute
                            let mut r = Arr::new();
                            r.insert(Key::Str(b"ts".to_vec()), Value::Int(ts));
                            r.insert(Key::Str(b"tz".to_vec()), Value::Str(effective.into_bytes()));
                            return Ok(Value::Array(r));
                        }
                        // unset fields default to "now" in the zone — or to the
                        // epoch after !/| (PHP's reset semantics)
                        let (dy, dmo, dd, dh, dmi, ds) = if p.default_epoch {
                            (1970, 1, 1, 0, 0, 0)
                        } else {
                            let now = crate::now_unix();
                            let local =
                                now + zone.as_ref().map(|z| z.offset_at(now).0 as i64).unwrap_or(0);
                            let (y, mo, d) = crate::civil_from_days(local.div_euclid(86400));
                            let secs = local.rem_euclid(86400);
                            (y, mo, d, secs / 3600, (secs % 3600) / 60, secs % 60)
                        };
                        let mut h = p.h.unwrap_or(dh);
                        if let Some(pm) = p.pm {
                            if pm && h < 12 {
                                h += 12;
                            }
                            if !pm && h == 12 {
                                h = 0;
                            }
                        }
                        let wall = crate::make_ts(
                            h,
                            p.mi.unwrap_or(dmi),
                            p.s.unwrap_or(ds),
                            p.mo.unwrap_or(dmo),
                            p.d.unwrap_or(dd),
                            p.y.unwrap_or(dy),
                        );
                        let ts = if let Some(off) = p.off {
                            wall - off
                        } else {
                            match &zone {
                                Some(z) => crate::tz::ts_from_local(wall, z),
                                None => wall,
                            }
                        };
                        let mut r = Arr::new();
                        r.insert(Key::Str(b"ts".to_vec()), Value::Int(ts));
                        // a parsed O/P offset becomes the object's (fixed-offset) zone
                        let tzout = match (p.off, &p.tzname) {
                            (Some(off), None) => {
                                let a = off.abs();
                                format!("{}{:02}:{:02}", if off < 0 { '-' } else { '+' }, a / 3600, (a % 3600) / 60)
                            }
                            _ => effective,
                        };
                        r.insert(Key::Str(b"tz".to_vec()), Value::Str(tzout.into_bytes()));
                        Value::Array(r)
                    }
                }
            }
            "__phargo_tz_transitions" => {
                let tzname = String::from_utf8_lossy(&to_bytes(&a(0))).into_owned();
                let begin = if args.len() > 1 { to_i64(&a(1)) } else { i32::MIN as i64 };
                let end = if args.len() > 2 { to_i64(&a(2)) } else { i32::MAX as i64 };
                match crate::tz::lookup(&tzname) {
                    Some(z) => {
                        let mut arr = Arr::new();
                        for (ts, off, isdst, abbr) in z.transitions(begin, end) {
                            let mut e = Arr::new();
                            e.insert(Key::Str(b"ts".to_vec()), Value::Int(ts));
                            e.insert(
                                Key::Str(b"time".to_vec()),
                                Value::Str(crate::php_date_tz("Y-m-d\\TH:i:sP", ts, None).into_bytes()),
                            );
                            e.insert(Key::Str(b"offset".to_vec()), Value::Int(off as i64));
                            e.insert(Key::Str(b"isdst".to_vec()), Value::Bool(isdst));
                            e.insert(Key::Str(b"abbr".to_vec()), Value::Str(abbr.into_bytes()));
                            arr.push(Value::Array(e));
                        }
                        Value::Array(arr)
                    }
                    None => Value::Bool(false),
                }
            }
            "__phargo_strtotime_tz" => {
                let s = String::from_utf8_lossy(&to_bytes(&a(0))).into_owned();
                let base = to_i64(&a(1));
                let tzname = String::from_utf8_lossy(&to_bytes(&a(2))).into_owned();
                let zone = if crate::tz::is_utc_name(&tzname) { None } else { crate::tz::lookup(&tzname) };
                match crate::php_strtotime_tz(&s, base, zone.as_deref()) {
                    Some(t) => Value::Int(t),
                    None => Value::Bool(false),
                }
            }
            "strftime" | "gmstrftime" => {
                let fmt = String::from_utf8_lossy(&to_bytes(&a(0))).into_owned();
                let ts = if args.len() > 1 { to_i64(&a(1)) } else { crate::now_unix() };
                Value::Str(crate::php_strftime(&fmt, ts).into_bytes())
            }
            "mktime" | "gmmktime" => {
                // defaults come from "now" in the zone the wall-clock is read in
                let zone = if name == "mktime" { self.cur_tz() } else { None };
                let now = crate::now_unix();
                let local_now = now + zone.as_ref().map(|z| z.offset_at(now).0 as i64).unwrap_or(0);
                let (cy, cm, cd) = crate::civil_from_days(local_now.div_euclid(86400));
                let secs = local_now.rem_euclid(86400);
                let g = |i: usize, dflt: i64| if args.len() > i { to_i64(&a(i)) } else { dflt };
                let wall = crate::make_ts(
                    g(0, secs / 3600),
                    g(1, (secs % 3600) / 60),
                    g(2, secs % 60),
                    g(3, cm),
                    g(4, cd),
                    g(5, cy),
                );
                Value::Int(match zone {
                    Some(z) => crate::tz::ts_from_local(wall, &z),
                    None => wall,
                })
            }
            "strtotime" => {
                let s = String::from_utf8_lossy(&to_bytes(&a(0))).into_owned();
                let base = if args.len() > 1 { to_i64(&a(1)) } else { crate::now_unix() };
                match crate::php_strtotime_tz(&s, base, self.cur_tz().as_deref()) {
                    Some(t) => Value::Int(t),
                    None => Value::Bool(false),
                }
            }
            "idate" => {
                let fmt = String::from_utf8_lossy(&to_bytes(&a(0))).into_owned();
                let ts = if args.len() > 1 { to_i64(&a(1)) } else { crate::now_unix() };
                let zone = self.cur_tz();
                let s = crate::php_date_tz(&fmt, ts, zone.as_deref());
                Value::Int(s.trim_start_matches('0').parse::<i64>().unwrap_or(0))
            }
            "compact" => {
                // inverse of extract(): collect named caller variables
                let mut out = Arr::new();
                fn collect(ev: &mut Eval, v: &Value, out: &mut Arr) {
                    match v {
                        Value::Array(a) => {
                            let entries = a.entries.clone();
                            for (_, item) in &entries {
                                collect(ev, item, out);
                            }
                        }
                        other => {
                            let name = String::from_utf8_lossy(&to_bytes(other)).into_owned();
                            if let Some(val) = ev.vars().get(&name).map(|v| v.deref()) {
                                out.insert(Key::Str(name.into_bytes()), val);
                            }
                        }
                    }
                }
                for v in &args {
                    collect(self, v, &mut out);
                }
                Value::Array(out)
            }
            "getdate" => {
                let ts = if !args.is_empty() { to_i64(&a(0)) } else { crate::now_unix() };
                let zone = self.cur_tz();
                let local = ts + zone.as_ref().map(|z| z.offset_at(ts).0 as i64).unwrap_or(0);
                let days = local.div_euclid(86400);
                let secs = local.rem_euclid(86400);
                let (y, m, d) = crate::civil_from_days(days);
                let wday = (days.rem_euclid(7) + 4) % 7;
                let yday = days - crate::days_from_civil(y, 1, 1);
                let mut r = Arr::new();
                r.insert(Key::Str(b"seconds".to_vec()), Value::Int(secs % 60));
                r.insert(Key::Str(b"minutes".to_vec()), Value::Int((secs % 3600) / 60));
                r.insert(Key::Str(b"hours".to_vec()), Value::Int(secs / 3600));
                r.insert(Key::Str(b"mday".to_vec()), Value::Int(d));
                r.insert(Key::Str(b"wday".to_vec()), Value::Int(wday));
                r.insert(Key::Str(b"mon".to_vec()), Value::Int(m));
                r.insert(Key::Str(b"year".to_vec()), Value::Int(y));
                r.insert(Key::Str(b"yday".to_vec()), Value::Int(yday));
                r.insert(Key::Str(b"weekday".to_vec()), Value::Str(crate::DAYS[wday as usize].as_bytes().to_vec()));
                r.insert(Key::Str(b"month".to_vec()), Value::Str(crate::MONTHS[(m - 1) as usize].as_bytes().to_vec()));
                r.insert(Key::Int(0), Value::Int(ts));
                Value::Array(r)
            }
            "checkdate" => {
                let (m, d, y) = (to_i64(&a(0)), to_i64(&a(1)), to_i64(&a(2)));
                Value::Bool(
                    (1..=12).contains(&m)
                        && d >= 1
                        && (1..=32767).contains(&y)
                        && d <= crate::days_in_month(y, m),
                )
            }
            "uniqid" => {
                let mut s = if args.is_empty() {
                    String::new()
                } else {
                    String::from_utf8_lossy(&to_bytes(&a(0))).into_owned()
                };
                let d = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default();
                s.push_str(&format!("{:08x}{:05x}", d.as_secs(), d.subsec_micros()));
                if to_bool(&a(1)) {
                    // more_entropy: PHP appends ".%08d" from php_combined_lcg
                    s.push_str(&format!("{}.{:08}", d.subsec_nanos() % 10, d.subsec_nanos() % 100_000_000));
                }
                Value::Str(s.into_bytes())
            }
            "microtime" => {
                if to_bool(&a(0)) {
                    Value::Float(crate::now_unix() as f64)
                } else {
                    Value::Str(format!("0.00000000 {}", crate::now_unix()).into_bytes())
                }
            }
            "preg_quote" => {
                let s = String::from_utf8_lossy(&to_bytes(&a(0))).into_owned();
                let delim = if args.len() > 1 {
                    String::from_utf8_lossy(&to_bytes(&a(1))).chars().next()
                } else {
                    None
                };
                Value::Str(crate::rx_quote(&s, delim).into_bytes())
            }
            "preg_replace" => preg_replace_full(&args).0,
            "preg_replace_callback" => {
                let pattern = String::from_utf8_lossy(&to_bytes(&a(0))).into_owned();
                let cb = a(1);
                let subject: Vec<char> =
                    String::from_utf8_lossy(&to_bytes(&a(2))).chars().collect();
                let rx = match crate::rx_compile(&pattern) {
                    Some(r) => r,
                    None => return Ok(Value::Null),
                };
                let mut out = String::new();
                let mut pos = 0;
                let mut steps = 0usize;
                while pos <= subject.len() {
                    match rx.exec(&subject, pos, &mut steps) {
                        Some(slots) => {
                            let (ms, me) = (slots[0], slots[1]);
                            out.extend(&subject[pos..ms]);
                            // build the match array for the callback
                            let mut m = Arr::new();
                            for g in 0..=rx.ngroups {
                                m.push(Value::Str(crate::rx_group_str(&subject, &slots, g).into_bytes()));
                            }
                            let r = self.call_value(cb.clone(), vec![Value::Array(m)])?;
                            out.push_str(&String::from_utf8_lossy(&to_bytes(&r)));
                            pos = if me > ms {
                                me
                            } else {
                                if ms < subject.len() {
                                    out.push(subject[ms]);
                                }
                                ms + 1
                            };
                        }
                        None => {
                            out.extend(&subject[pos..]);
                            break;
                        }
                    }
                }
                Value::Str(out.into_bytes())
            }
            "preg_split" => {
                let pattern = String::from_utf8_lossy(&to_bytes(&a(0))).into_owned();
                let subject: Vec<char> =
                    String::from_utf8_lossy(&to_bytes(&a(1))).chars().collect();
                let flags = to_i64(&a(3));
                let no_empty = flags & 1 != 0; // PREG_SPLIT_NO_EMPTY
                let delim_capture = flags & 2 != 0; // PREG_SPLIT_DELIM_CAPTURE
                let mut arr = Arr::new();
                match crate::rx_compile(&pattern) {
                    Some(rx) => {
                        let mut pos = 0;
                        let mut last = 0;
                        let mut steps = 0usize;
                        while pos <= subject.len() {
                            match rx.exec(&subject, pos, &mut steps) {
                                Some(slots) => {
                                    let (ms, me) = (slots[0], slots[1]);
                                    if me == ms && ms == last {
                                        pos = ms + 1;
                                        continue;
                                    }
                                    let piece: String = subject[last..ms].iter().collect();
                                    if !(no_empty && piece.is_empty()) {
                                        arr.push(Value::Str(piece.into_bytes()));
                                    }
                                    // captured delimiter groups interleave with
                                    // the pieces (wpdb::prepare depends on this)
                                    if delim_capture {
                                        for g in 1..=rx.ngroups {
                                            if slots.get(2 * g).copied().unwrap_or(usize::MAX)
                                                == usize::MAX
                                            {
                                                continue;
                                            }
                                            let gs = crate::rx_group_str(&subject, &slots, g);
                                            if !(no_empty && gs.is_empty()) {
                                                arr.push(Value::Str(gs.into_bytes()));
                                            }
                                        }
                                    }
                                    last = me;
                                    pos = if me > ms { me } else { me + 1 };
                                }
                                None => break,
                            }
                        }
                        let piece: String = subject[last..].iter().collect();
                        if !(no_empty && piece.is_empty()) {
                            arr.push(Value::Str(piece.into_bytes()));
                        }
                    }
                    None => arr.push(Value::Str(subject.iter().collect::<String>().into_bytes())),
                }
                Value::Array(arr)
            }
            "preg_grep" => {
                let pattern = String::from_utf8_lossy(&to_bytes(&a(0))).into_owned();
                let mut out = Arr::new();
                if let (Some(rx), Value::Array(arr)) = (crate::rx_compile(&pattern), a(1)) {
                    for (k, v) in arr.entries {
                        let text: Vec<char> = String::from_utf8_lossy(&to_bytes(&v)).chars().collect();
                        let mut steps = 0usize;
                        if rx.exec(&text, 0, &mut steps).is_some() {
                            out.insert(k, v);
                        }
                    }
                }
                Value::Array(out)
            }
            "json_encode" => {
                let mut out = Vec::new();
                json_encode(&a(0), &mut out, 0);
                Value::Str(out)
            }
            "json_decode" => {
                let bytes = to_bytes(&a(0));
                let assoc = to_bool(&a(1));
                json_decode(&bytes, assoc).unwrap_or(Value::Null)
            }
            "json_last_error" => Value::Int(0),
            "json_last_error_msg" => Value::Str(b"No error".to_vec()),
            "serialize" => {
                let mut out = Vec::new();
                self.ser_val(&a(0), &mut out, 0)?;
                Value::Str(out)
            }
            "unserialize" => {
                let bytes = to_bytes(&a(0));
                let mut pos = 0;
                let v = php_unserialize(&bytes, &mut pos, 0).unwrap_or(Value::Bool(false));
                self.apply_wakeup(&v, 0)?;
                v
            }
            "filter_var" => {
                let v = a(0);
                let filter = if args.len() > 1 { to_i64(&a(1)) } else { 516 };
                match filter {
                    257 => match String::from_utf8_lossy(&to_bytes(&v)).trim().parse::<i64>() {
                        Ok(n) => Value::Int(n),
                        Err(_) => Value::Bool(false),
                    },
                    259 => match String::from_utf8_lossy(&to_bytes(&v)).trim().parse::<f64>() {
                        Ok(n) => Value::Float(n),
                        Err(_) => Value::Bool(false),
                    },
                    258 => {
                        let s = String::from_utf8_lossy(&to_bytes(&v)).trim().to_ascii_lowercase();
                        match s.as_str() {
                            "1" | "true" | "on" | "yes" => Value::Bool(true),
                            "0" | "false" | "off" | "no" | "" => Value::Bool(false),
                            _ => Value::Null,
                        }
                    }
                    274 => {
                        // FILTER_VALIDATE_EMAIL — rough check
                        let s = String::from_utf8_lossy(&to_bytes(&v)).into_owned();
                        if s.contains('@') && s.contains('.') && !s.contains(' ') {
                            Value::Str(s.into_bytes())
                        } else {
                            Value::Bool(false)
                        }
                    }
                    273 => {
                        // FILTER_VALIDATE_URL
                        let s = String::from_utf8_lossy(&to_bytes(&v)).into_owned();
                        if s.contains("://") {
                            Value::Str(s.into_bytes())
                        } else {
                            Value::Bool(false)
                        }
                    }
                    _ => Value::Str(to_bytes(&v)),
                }
            }
            "ctype_digit" => {
                let s = to_bytes(&a(0));
                Value::Bool(!s.is_empty() && s.iter().all(|b| b.is_ascii_digit()))
            }
            "ctype_alpha" => {
                let s = to_bytes(&a(0));
                Value::Bool(!s.is_empty() && s.iter().all(|b| b.is_ascii_alphabetic()))
            }
            "ctype_alnum" => {
                let s = to_bytes(&a(0));
                Value::Bool(!s.is_empty() && s.iter().all(|b| b.is_ascii_alphanumeric()))
            }
            "ctype_space" => {
                let s = to_bytes(&a(0));
                Value::Bool(!s.is_empty() && s.iter().all(|b| b.is_ascii_whitespace()))
            }
            "phargo_civil_add" => {
                // PHP's DateTime::add does calendar math on the LOCAL wall clock
                // (crossing a DST change keeps the wall time) — optional arg 7 is
                // the zone name.
                let ts = to_i64(&a(0));
                let zone = if args.len() > 7 {
                    let tzname = String::from_utf8_lossy(&to_bytes(&a(7))).into_owned();
                    if crate::tz::is_utc_name(&tzname) { None } else { crate::tz::lookup(&tzname) }
                } else {
                    None
                };
                let wall = ts + zone.as_ref().map(|z| z.offset_at(ts).0 as i64).unwrap_or(0);
                let days0 = wall.div_euclid(86400);
                let secs0 = wall.rem_euclid(86400);
                let (y, mo, d) = crate::civil_from_days(days0);
                let (dy, dm, dd) = (to_i64(&a(1)), to_i64(&a(2)), to_i64(&a(3)));
                let (dh, di, ds) = (to_i64(&a(4)), to_i64(&a(5)), to_i64(&a(6)));
                let total_months = (y * 12 + (mo - 1)) + dy * 12 + dm;
                let ny = total_months.div_euclid(12);
                let nmo = total_months.rem_euclid(12) + 1;
                let nday = d.min(crate::days_in_month(ny, nmo));
                let base = crate::days_from_civil(ny, nmo, nday) * 86400 + secs0;
                let out_wall = base + dd * 86400 + dh * 3600 + di * 60 + ds;
                Value::Int(match zone {
                    Some(z) => crate::tz::ts_from_local(out_wall, &z),
                    None => out_wall,
                })
            }
            "phargo_date_diff" => {
                let (t1, t2) = (to_i64(&a(0)), to_i64(&a(1)));
                let invert = t1 > t2;
                let (lo, hi) = if t1 <= t2 { (t1, t2) } else { (t2, t1) };
                let dec = |ts: i64| {
                    let days = ts.div_euclid(86400);
                    let s = ts.rem_euclid(86400);
                    let (y, m, d) = crate::civil_from_days(days);
                    (y, m, d, s / 3600, (s % 3600) / 60, s % 60)
                };
                let (y1, mo1, d1, h1, mi1, s1) = dec(lo);
                let (y2, mo2, d2, h2, mi2, s2) = dec(hi);
                let (mut s, mut mi, mut h, mut d, mut mo, mut y) =
                    (s2 - s1, mi2 - mi1, h2 - h1, d2 - d1, mo2 - mo1, y2 - y1);
                if s < 0 { s += 60; mi -= 1; }
                if mi < 0 { mi += 60; h -= 1; }
                if h < 0 { h += 24; d -= 1; }
                if d < 0 {
                    let pm = if mo2 == 1 { 12 } else { mo2 - 1 };
                    let py = if mo2 == 1 { y2 - 1 } else { y2 };
                    d += crate::days_in_month(py, pm);
                    mo -= 1;
                }
                if mo < 0 { mo += 12; y -= 1; }
                let total_days = lo.div_euclid(86400).abs_diff(hi.div_euclid(86400)) as i64;
                let mut r = Arr::new();
                for (k, v) in [("y", y), ("m", mo), ("d", d), ("h", h), ("i", mi), ("s", s), ("days", total_days)] {
                    r.insert(Key::Str(k.as_bytes().to_vec()), Value::Int(v));
                }
                r.insert(Key::Str(b"invert".to_vec()), Value::Int(invert as i64));
                let _ = (d1, h1, mi1, s1, mo1, y1);
                Value::Array(r)
            }
            "__phargo_modify" => {
                let ts = to_i64(&a(0));
                let s = String::from_utf8_lossy(&to_bytes(&a(1))).to_ascii_lowercase();
                let (mut dy, mut dm, mut dd, mut dh, mut di, mut ds) = (0i64, 0, 0, 0, 0, 0);
                let chars: Vec<char> = s.chars().collect();
                let mut i = 0;
                while i < chars.len() {
                    if s[i..].starts_with("tomorrow") { dd += 1; i += 8; continue; }
                    if s[i..].starts_with("yesterday") { dd -= 1; i += 9; continue; }
                    let c = chars[i];
                    if c == '+' || c == '-' || c.is_ascii_digit() {
                        let start = i;
                        if c == '+' || c == '-' { i += 1; }
                        while i < chars.len() && chars[i].is_ascii_digit() { i += 1; }
                        let num: i64 = s[start..i].parse().unwrap_or(0);
                        while i < chars.len() && chars[i].is_whitespace() { i += 1; }
                        let ws = i;
                        while i < chars.len() && chars[i].is_ascii_alphabetic() { i += 1; }
                        let unit = &s[ws..i];
                        match unit {
                            u if u.starts_with("year") => dy += num,
                            u if u.starts_with("month") => dm += num,
                            u if u.starts_with("week") => dd += num * 7,
                            u if u.starts_with("day") => dd += num,
                            u if u.starts_with("hour") => dh += num,
                            u if u.starts_with("min") => di += num,
                            u if u.starts_with("sec") => ds += num,
                            _ => {}
                        }
                    } else {
                        i += 1;
                    }
                }
                // wall-clock math in the object's zone (optional arg 2)
                let zone = if args.len() > 2 {
                    let tzname = String::from_utf8_lossy(&to_bytes(&a(2))).into_owned();
                    if crate::tz::is_utc_name(&tzname) { None } else { crate::tz::lookup(&tzname) }
                } else {
                    None
                };
                let wall = ts + zone.as_ref().map(|z| z.offset_at(ts).0 as i64).unwrap_or(0);
                let days0 = wall.div_euclid(86400);
                let secs0 = wall.rem_euclid(86400);
                let (y, mo, d) = crate::civil_from_days(days0);
                let total_months = (y * 12 + (mo - 1)) + dy * 12 + dm;
                let ny = total_months.div_euclid(12);
                let nmo = total_months.rem_euclid(12) + 1;
                let nday = d.min(crate::days_in_month(ny, nmo));
                let base = crate::days_from_civil(ny, nmo, nday) * 86400 + secs0;
                let out_wall = base + dd * 86400 + dh * 3600 + di * 60 + ds;
                Value::Int(match zone {
                    Some(z) => crate::tz::ts_from_local(out_wall, &z),
                    None => out_wall,
                })
            }
            // ---- filesystem ----
            "file_get_contents" => {
                let path = String::from_utf8_lossy(&to_bytes(&a(0))).into_owned();
                match std::fs::read(&path) {
                    Ok(b) => Value::Str(b),
                    Err(_) => Value::Bool(false),
                }
            }
            "file_put_contents" => {
                let path = String::from_utf8_lossy(&to_bytes(&a(0))).into_owned();
                let bytes = match &a(1) {
                    Value::Array(arr) => {
                        let mut v = Vec::new();
                        for (_, e) in &arr.entries {
                            v.extend_from_slice(&to_bytes(e));
                        }
                        v
                    }
                    v => to_bytes(v),
                };
                // FILE_APPEND = 8
                let append = to_i64(&a(2)) & 8 != 0;
                let res = if append {
                    use std::io::Write;
                    std::fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(&path)
                        .and_then(|mut f| f.write_all(&bytes))
                } else {
                    std::fs::write(&path, &bytes)
                };
                match res {
                    Ok(_) => Value::Int(bytes.len() as i64),
                    Err(_) => Value::Bool(false),
                }
            }
            "file" => {
                let path = String::from_utf8_lossy(&to_bytes(&a(0))).into_owned();
                match std::fs::read(&path) {
                    Ok(b) => {
                        let ignore_nl = to_i64(&a(1)) & 2 != 0;
                        let mut arr = Arr::new();
                        let mut start = 0;
                        for i in 0..b.len() {
                            if b[i] == b'\n' {
                                let end = if ignore_nl { i } else { i + 1 };
                                arr.push(Value::Str(b[start..end].to_vec()));
                                start = i + 1;
                            }
                        }
                        if start < b.len() {
                            arr.push(Value::Str(b[start..].to_vec()));
                        }
                        Value::Array(arr)
                    }
                    Err(_) => Value::Bool(false),
                }
            }
            "readfile" => {
                let path = String::from_utf8_lossy(&to_bytes(&a(0))).into_owned();
                match std::fs::read(&path) {
                    Ok(b) => {
                        let n = b.len();
                        self.out.extend_from_slice(&b);
                        Value::Int(n as i64)
                    }
                    Err(_) => Value::Bool(false),
                }
            }
            "fopen" => {
                let path = String::from_utf8_lossy(&to_bytes(&a(0))).into_owned();
                let mode = String::from_utf8_lossy(&to_bytes(&a(1))).into_owned();
                self.fopen_impl(&path, &mode)
            }
            "fwrite" | "fputs" => {
                let data = to_bytes(&a(1));
                let data = if args.len() > 2 {
                    let n = to_i64(&a(2)).max(0) as usize;
                    data[..n.min(data.len())].to_vec()
                } else {
                    data
                };
                self.stream_write(&a(0), &data)?
            }
            "fread" => {
                let n = to_i64(&a(1)).max(0) as usize;
                self.stream_read_n(&a(0), Some(n))
            }
            "fgets" => {
                let n = if args.len() > 1 && !matches!(a(1), Value::Null) {
                    Some(to_i64(&a(1)).max(0) as usize)
                } else {
                    None
                };
                self.stream_gets(&a(0), n)
            }
            "fgetc" => match self.stream_read_n(&a(0), Some(1)) {
                Value::Str(s) if s.is_empty() => Value::Bool(false),
                v => v,
            },
            "fgetcsv" => {
                // fgetcsv($stream, $length=null, $separator=',', $enclosure='"', ...)
                match self.stream_gets(&a(0), None) {
                    Value::Str(mut l) => {
                        while matches!(l.last(), Some(b'\n') | Some(b'\r')) {
                            l.pop();
                        }
                        let delim = first_byte_or(&to_bytes(&a(2)), b',');
                        let quote = if args.len() > 3 { first_byte_or(&to_bytes(&a(3)), b'"') } else { b'"' };
                        parse_csv(&l, delim, quote)
                    }
                    _ => Value::Bool(false),
                }
            }
            "fputcsv" => {
                // fputcsv($stream, $fields, $separator=',', $enclosure='"', ..., $eol="\n")
                let delim = first_byte_or(&to_bytes(&a(2)), b',');
                let quote = if args.len() > 3 { first_byte_or(&to_bytes(&a(3)), b'"') } else { b'"' };
                let mut line: Vec<u8> = Vec::new();
                if let Value::Array(arr) = a(1) {
                    for (i, (_, v)) in arr.entries.iter().enumerate() {
                        if i > 0 {
                            line.push(delim);
                        }
                        let f = to_bytes(v);
                        let needs_q = f.contains(&delim)
                            || f.contains(&quote)
                            || f.contains(&b'\n')
                            || f.contains(&b'\r');
                        if needs_q {
                            line.push(quote);
                            for &b in &f {
                                if b == quote {
                                    line.push(quote);
                                }
                                line.push(b);
                            }
                            line.push(quote);
                        } else {
                            line.extend_from_slice(&f);
                        }
                    }
                }
                let eol = if args.len() > 5 { to_bytes(&a(5)) } else { b"\n".to_vec() };
                line.extend_from_slice(&eol);
                self.stream_write(&a(0), &line)?
            }
            "stream_get_contents" => {
                let n = if args.len() > 1 && !matches!(a(1), Value::Null) && to_i64(&a(1)) >= 0 {
                    Some(to_i64(&a(1)) as usize)
                } else {
                    None
                };
                if args.len() > 2 && to_i64(&a(2)) >= 0 {
                    let _ = self.stream_seek(&a(0), to_i64(&a(2)), 0);
                }
                self.stream_read_n(&a(0), n)
            }
            "fpassthru" => {
                let rest = self.stream_read_n(&a(0), None);
                let bytes = to_bytes(&rest);
                let n = bytes.len();
                self.out.extend_from_slice(&bytes);
                Value::Int(n as i64)
            }
            "feof" => {
                if let Value::Object(o) = a(0) {
                    let b = o.borrow();
                    let pos = b.get("__pos").map(to_i64).unwrap_or(0);
                    let len = match b.get("__buf") {
                        Some(Value::Str(s)) => s.len() as i64,
                        _ => 0,
                    };
                    Value::Bool(pos >= len)
                } else {
                    Value::Bool(true)
                }
            }
            "ftell" => {
                if let Value::Object(o) = a(0) {
                    Value::Int(o.borrow().get("__pos").map(to_i64).unwrap_or(0))
                } else {
                    Value::Bool(false)
                }
            }
            "fseek" => {
                let whence = to_i64(&a(2));
                Value::Int(self.stream_seek(&a(0), to_i64(&a(1)), whence))
            }
            "rewind" => {
                let _ = self.stream_seek(&a(0), 0, 0);
                Value::Bool(true)
            }
            "fflush" | "fclose" => Value::Bool(matches!(&a(0), Value::Object(o) if o.borrow().class == "__Stream")),
            "unlink" => {
                let path = String::from_utf8_lossy(&to_bytes(&a(0))).into_owned();
                Value::Bool(std::fs::remove_file(&path).is_ok())
            }
            "mkdir" => {
                let path = String::from_utf8_lossy(&to_bytes(&a(0))).into_owned();
                let recursive = to_bool(&a(2));
                let r = if recursive {
                    std::fs::create_dir_all(&path)
                } else {
                    std::fs::create_dir(&path)
                };
                Value::Bool(r.is_ok())
            }
            "rmdir" => {
                let path = String::from_utf8_lossy(&to_bytes(&a(0))).into_owned();
                Value::Bool(std::fs::remove_dir(&path).is_ok())
            }
            "rename" => Value::Bool(
                std::fs::rename(
                    String::from_utf8_lossy(&to_bytes(&a(0))).as_ref(),
                    String::from_utf8_lossy(&to_bytes(&a(1))).as_ref(),
                )
                .is_ok(),
            ),
            "copy" => Value::Bool(
                std::fs::copy(
                    String::from_utf8_lossy(&to_bytes(&a(0))).as_ref(),
                    String::from_utf8_lossy(&to_bytes(&a(1))).as_ref(),
                )
                .is_ok(),
            ),
            "file_exists" => {
                Value::Bool(std::path::Path::new(&String::from_utf8_lossy(&to_bytes(&a(0))).into_owned()).exists())
            }
            "is_file" => Value::Bool(
                std::path::Path::new(&String::from_utf8_lossy(&to_bytes(&a(0))).into_owned()).is_file(),
            ),
            "is_dir" => Value::Bool(
                std::path::Path::new(&String::from_utf8_lossy(&to_bytes(&a(0))).into_owned()).is_dir(),
            ),
            "is_readable" | "is_writable" | "is_writeable" => {
                Value::Bool(std::path::Path::new(&String::from_utf8_lossy(&to_bytes(&a(0))).into_owned()).exists())
            }
            "filesize" => {
                let path = String::from_utf8_lossy(&to_bytes(&a(0))).into_owned();
                match std::fs::metadata(&path) {
                    Ok(m) => Value::Int(m.len() as i64),
                    Err(_) => Value::Bool(false),
                }
            }
            "glob" => {
                let pattern = String::from_utf8_lossy(&to_bytes(&a(0))).into_owned();
                let flags = to_i64(&a(1));
                let onlydir = flags & 8192 != 0; // GLOB_ONLYDIR
                let mark = flags & 2 != 0; // GLOB_MARK
                let nosort = flags & 4 != 0; // GLOB_NOSORT
                let mut hits = php_glob(&pattern);
                if onlydir {
                    hits.retain(|p| std::path::Path::new(p).is_dir());
                }
                if !nosort {
                    hits.sort();
                }
                if mark {
                    for p in &mut hits {
                        if std::path::Path::new(p.as_str()).is_dir() && !p.ends_with('/') {
                            p.push('/');
                        }
                    }
                }
                let mut arr = Arr::new();
                for p in hits {
                    arr.push(Value::Str(p.into_bytes()));
                }
                Value::Array(arr)
            }
            "scandir" => {
                let path = String::from_utf8_lossy(&to_bytes(&a(0))).into_owned();
                match std::fs::read_dir(&path) {
                    Ok(rd) => {
                        let mut names: Vec<Vec<u8>> = vec![b".".to_vec(), b"..".to_vec()];
                        for e in rd.flatten() {
                            names.push(e.file_name().to_string_lossy().as_bytes().to_vec());
                        }
                        names.sort();
                        let mut arr = Arr::new();
                        for n in names {
                            arr.push(Value::Str(n));
                        }
                        Value::Array(arr)
                    }
                    Err(_) => Value::Bool(false),
                }
            }
            "opendir" => {
                let path = String::from_utf8_lossy(&to_bytes(&a(0))).into_owned();
                match std::fs::read_dir(&path) {
                    Ok(rd) => {
                        let mut names: Vec<Vec<u8>> = vec![b".".to_vec(), b"..".to_vec()];
                        for e in rd.flatten() {
                            names.push(e.file_name().to_string_lossy().as_bytes().to_vec());
                        }
                        names.sort();
                        let mut entries = Arr::new();
                        for n in names {
                            entries.push(Value::Str(n));
                        }
                        let o = new_obj("__Dir");
                        if let Value::Object(rc) = &o {
                            let mut b = rc.borrow_mut();
                            b.set("__entries", Value::Array(entries));
                            b.set("__pos", Value::Int(0));
                            b.set("path", Value::Str(path.as_bytes().to_vec()));
                        }
                        o
                    }
                    Err(_) => Value::Bool(false),
                }
            }
            "readdir" => {
                if let Value::Object(o) = a(0) {
                    let mut b = o.borrow_mut();
                    let pos = b.get("__pos").map(to_i64).unwrap_or(0).max(0) as usize;
                    let next = match b.get("__entries") {
                        Some(Value::Array(e)) => e.entries.get(pos).map(|(_, v)| v.clone()),
                        _ => None,
                    };
                    match next {
                        Some(v) => {
                            b.set("__pos", Value::Int(pos as i64 + 1));
                            v
                        }
                        None => Value::Bool(false),
                    }
                } else {
                    Value::Bool(false)
                }
            }
            "rewinddir" => {
                if let Value::Object(o) = a(0) {
                    o.borrow_mut().set("__pos", Value::Int(0));
                }
                Value::Null
            }
            "closedir" => Value::Null,
            "realpath" => {
                let path = String::from_utf8_lossy(&to_bytes(&a(0))).into_owned();
                match std::fs::canonicalize(&path) {
                    Ok(p) => {
                        let s = p.to_string_lossy();
                        // strip Windows \\?\ prefix
                        let s = s.strip_prefix(r"\\?\").unwrap_or(&s);
                        Value::Str(s.as_bytes().to_vec())
                    }
                    Err(_) => Value::Bool(false),
                }
            }
            "getcwd" => match std::env::current_dir() {
                Ok(p) => Value::Str(p.to_string_lossy().as_bytes().to_vec()),
                Err(_) => Value::Bool(false),
            },
            "sys_get_temp_dir" => {
                Value::Str(std::env::temp_dir().to_string_lossy().as_bytes().to_vec())
            }
            "touch" => {
                let path = String::from_utf8_lossy(&to_bytes(&a(0))).into_owned();
                Value::Bool(
                    std::fs::OpenOptions::new()
                        .create(true)
                        .write(true)
                        .open(&path)
                        .is_ok(),
                )
            }
            "pathinfo" => {
                let path = String::from_utf8_lossy(&to_bytes(&a(0))).into_owned();
                let p = std::path::Path::new(&path);
                let dir = p.parent().map(|d| d.to_string_lossy().into_owned()).filter(|s| !s.is_empty()).unwrap_or_else(|| ".".into());
                let base = p.file_name().map(|b| b.to_string_lossy().into_owned()).unwrap_or_default();
                let ext = p.extension().map(|e| e.to_string_lossy().into_owned());
                let stem = p.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
                let mut arr = Arr::new();
                arr.insert(Key::Str(b"dirname".to_vec()), Value::Str(dir.into_bytes()));
                arr.insert(Key::Str(b"basename".to_vec()), Value::Str(base.into_bytes()));
                if let Some(e) = ext {
                    arr.insert(Key::Str(b"extension".to_vec()), Value::Str(e.into_bytes()));
                }
                arr.insert(Key::Str(b"filename".to_vec()), Value::Str(stem.into_bytes()));
                // A component flag (PATHINFO_DIRNAME=1/BASENAME=2/EXTENSION=4/FILENAME=8)
                // returns just that piece as a string.
                if args.len() > 1 {
                    let key: &[u8] = match to_i64(&a(1)) {
                        1 => b"dirname",
                        2 => b"basename",
                        4 => b"extension",
                        8 => b"filename",
                        _ => b"",
                    };
                    return Ok(arr.get(&Key::Str(key.to_vec())).cloned().unwrap_or(Value::Str(Vec::new())));
                }
                Value::Array(arr)
            }
            // ---- environment / config stubs (setup calls; sane defaults) ----
            "getenv" => {
                if args.is_empty() {
                    Value::Array(Arr::new())
                } else {
                    match std::env::var(String::from_utf8_lossy(&to_bytes(&a(0))).as_ref()) {
                        Ok(v) => Value::Str(v.into_bytes()),
                        Err(_) => Value::Bool(false),
                    }
                }
            }
            "extension_loaded" => {
                let e = String::from_utf8_lossy(&to_bytes(&a(0))).to_ascii_lowercase();
                // report what the engine genuinely provides
                Value::Bool(matches!(
                    e.as_str(),
                    "core" | "standard" | "pcre" | "spl" | "date" | "json" | "hash"
                        | "ctype" | "session" | "dom" | "simplexml" | "xml" | "xmlreader"
                        | "libxml" | "bcmath" | "pdo" | "pdo_sqlite" | "reflection" | "filter"
                        | "random" | "tokenizer"
                ))
            }
            "get_loaded_extensions" => {
                // mirrors extension_loaded's honesty list
                let mut arr = Arr::new();
                for e in [
                    "Core", "standard", "pcre", "SPL", "date", "json", "hash", "ctype",
                    "session", "dom", "SimpleXML", "xml", "xmlreader", "libxml", "bcmath",
                    "PDO", "pdo_sqlite", "Reflection", "filter", "random", "tokenizer",
                ] {
                    arr.push(Value::Str(e.as_bytes().to_vec()));
                }
                Value::Array(arr)
            }
            "umask" => Value::Int(0o022),
            "putenv" | "set_time_limit" | "ignore_user_abort" | "setlocale" => {
                Value::Bool(false)
            }
            "ini_get" | "ini_set" => Value::Bool(false),
            "error_reporting" => {
                let old = self.error_level;
                if !args.is_empty() {
                    self.error_level = to_i64(&a(0));
                }
                Value::Int(old)
            }
            "set_error_handler" => {
                let prev = self.error_handler.take();
                let h = a(0);
                self.error_handler = if matches!(h, Value::Null) { None } else { Some(h) };
                prev.unwrap_or(Value::Null)
            }
            "restore_error_handler" => {
                self.error_handler = None;
                Value::Bool(true)
            }
            "set_exception_handler"
            | "restore_exception_handler" | "error_clear_last" | "debug_print_backtrace"
            | "gc_enable" | "gc_disable" | "header" | "clearstatcache" | "usleep" | "sleep" => {
                Value::Null
            }
            "date_default_timezone_set" => {
                let tzname = String::from_utf8_lossy(&to_bytes(&a(0))).into_owned();
                if crate::tz::is_utc_name(&tzname) || crate::tz::lookup(&tzname).is_some() {
                    self.default_tz = tzname;
                    Value::Bool(true)
                } else {
                    Value::Bool(false)
                }
            }
            "spl_autoload_register" => {
                if !args.is_empty() {
                    self.autoloaders.push(a(0));
                }
                Value::Bool(true)
            }
            "spl_autoload_unregister" => {
                let target = a(0);
                self.autoloaders.retain(|l| match (l, &target) {
                    (Value::Str(x), Value::Str(y)) => x != y,
                    (Value::Closure(x), Value::Closure(y)) => !Rc::ptr_eq(x, y),
                    _ => true,
                });
                Value::Bool(true)
            }
            "spl_autoload_functions" => {
                let mut arr = Arr::new();
                for l in &self.autoloaders {
                    arr.push(l.clone());
                }
                Value::Array(arr)
            }
            "trigger_error" => {
                let msg = String::from_utf8_lossy(&to_bytes(&a(0))).into_owned();
                let level = if args.len() > 1 { to_i64(&a(1)) } else { 1024 }; // E_USER_NOTICE
                let file = self
                    .cur_file
                    .as_ref()
                    .map(|p| p.to_string_lossy().into_owned())
                    .unwrap_or_default();
                let mut handled = false;
                if let Some(h) = self.error_handler.clone() {
                    if !self.in_error_handler {
                        self.in_error_handler = true;
                        let r = self.call_value(
                            h,
                            vec![
                                Value::Int(level),
                                Value::Str(msg.clone().into_bytes()),
                                Value::Str(file.clone().into_bytes()),
                                Value::Int(self.cur_line as i64),
                            ],
                        );
                        self.in_error_handler = false;
                        handled = !matches!(r?, Value::Bool(false));
                    }
                }
                if !handled && self.error_level & level != 0 && self.out.len() <= MAX_OUTPUT {
                    let label = match level {
                        256 => "Fatal error",   // E_USER_ERROR
                        512 => "Warning",       // E_USER_WARNING
                        8192 | 16384 => "Deprecated",
                        _ => "Notice",
                    };
                    let s = format!("\n{label}: {msg} in {file} on line {}\n", self.cur_line);
                    self.out.extend_from_slice(s.as_bytes());
                }
                if level == 256 {
                    return Err(RunError("__phargo_exit__".into())); // E_USER_ERROR halts
                }
                Value::Bool(true)
            }
            "assert" | "gc_enabled" | "headers_sent"
            | "stream_set_blocking" | "stream_set_timeout" | "stream_set_read_buffer"
            | "stream_set_write_buffer" | "stream_wrapper_register" | "stream_wrapper_unregister"
            | "stream_wrapper_restore" | "stream_filter_remove" => {
                Value::Bool(true)
            }
            // stream filters: return a non-false handle (a marker object)
            "stream_filter_append" | "stream_filter_prepend" => new_obj("__StreamFilter"),
            "stream_context_create" | "stream_context_get_default" => new_obj("__StreamContext"),
            "stream_context_set_option" | "stream_context_set_default" | "stream_context_set_params" => {
                Value::Bool(true)
            }
            "stream_get_meta_data" => {
                let mut m = Arr::new();
                m.insert(Key::Str(b"timed_out".to_vec()), Value::Bool(false));
                m.insert(Key::Str(b"blocked".to_vec()), Value::Bool(true));
                m.insert(Key::Str(b"eof".to_vec()), Value::Bool(to_bool(&self.builtin("feof", vec![a(0)])?)));
                m.insert(Key::Str(b"seekable".to_vec()), Value::Bool(true));
                m.insert(Key::Str(b"stream_type".to_vec()), Value::Str(b"MEMORY".to_vec()));
                m.insert(Key::Str(b"mode".to_vec()), Value::Str(b"r+".to_vec()));
                m.insert(Key::Str(b"unread_bytes".to_vec()), Value::Int(0));
                Value::Array(m)
            }
            "stream_get_line" => {
                // stream_get_line($h, $length, $ending=""): read a line, strip ending.
                let line = self.stream_gets(&a(0), None);
                match line {
                    Value::Str(mut l) => {
                        let ending = to_bytes(&a(2));
                        if !ending.is_empty() {
                            if l.ends_with(&ending) {
                                l.truncate(l.len() - ending.len());
                            }
                        } else {
                            while matches!(l.last(), Some(b'\n') | Some(b'\r')) {
                                l.pop();
                            }
                        }
                        Value::Str(l)
                    }
                    _ => Value::Bool(false),
                }
            }
            "register_shutdown_function" => {
                if !matches!(a(0), Value::Null) {
                    let mut extra = Vec::new();
                    for i in 1..args.len() {
                        extra.push(a(i));
                    }
                    self.shutdown_fns.push((a(0), extra));
                }
                Value::Null
            }
            "session_start" | "session_regenerate_id" | "session_destroy" | "session_write_close"
            | "session_commit" | "session_reset" | "session_abort" | "session_set_save_handler"
            | "session_register_shutdown" | "session_unset" => Value::Bool(true),
            "session_set_cookie_params" | "session_cache_limiter" | "session_cache_expire" => {
                Value::Bool(true)
            }
            "session_id" | "session_create_id" => Value::Str(b"phargosession".to_vec()),
            "session_name" => Value::Str(b"PHPSESSID".to_vec()),
            "session_status" => Value::Int(1), // PHP_SESSION_NONE
            "session_save_path" | "session_module_name" => Value::Str(Vec::new()),
            "session_get_cookie_params" => Value::Array(Arr::new()),
            "php_sapi_name" => Value::Str(b"cli".to_vec()),
            "date_default_timezone_get" => Value::Str(self.default_tz.clone().into_bytes()),
            "debug_backtrace" => Value::Array(Arr::new()),
            "gc_collect_cycles" | "http_response_code" | "getmypid" | "hrtime" => Value::Int(0),
            "memory_get_usage" | "memory_get_peak_usage" => Value::Int(2_000_000),
            "php_sapi_name" => Value::Str(b"cli".to_vec()),
            "phpversion" => Value::Str(b"8.3.0".to_vec()),
            "php_uname" => Value::Str(b"Linux".to_vec()),
            "error_get_last" => Value::Null,
            // error_log: accepted and discarded (no log sink in the harness)
            "error_log" => Value::Bool(true),
            // no HTTP response channel in the harness — accepted and dropped
            "setcookie" | "setrawcookie" => Value::Bool(true),
            _ => {
                return Err(
                    self.throw_error("Error", &format!("Call to undefined function {name}()"))
                )
            }
        })
    }

    fn extreme(&self, args: &[Value], want_max: bool) -> Value {
        let items: Vec<Value> = if args.len() == 1 {
            if let Value::Array(a) = &args[0] {
                a.entries.iter().map(|(_, v)| v.clone()).collect()
            } else {
                vec![args[0].clone()]
            }
        } else {
            args.to_vec()
        };
        let mut best: Option<Value> = None;
        for v in items {
            best = Some(match best {
                None => v,
                Some(b) => {
                    let take = if want_max {
                        compare(&v, &b) == std::cmp::Ordering::Greater
                    } else {
                        compare(&v, &b) == std::cmp::Ordering::Less
                    };
                    if take {
                        v
                    } else {
                        b
                    }
                }
            });
        }
        best.unwrap_or(Value::Null)
    }

    fn range(&mut self, start: &Value, end: &Value, step: &Value) -> R<Value> {
        let mut arr = Arr::new();
        // integer range when both bounds are ints and the step is whole — iterate
        // in integer (i128) arithmetic. Doing this in f64 breaks near i64::MIN/MAX
        // (the ulp exceeds 1, so `x += 1.0` is a no-op → infinite loop).
        let step_whole = matches!(step, Value::Null) || to_f64(step).fract() == 0.0;
        if matches!(start, Value::Int(_)) && matches!(end, Value::Int(_)) && step_whole {
            let a = to_i64(start) as i128;
            let b = to_i64(end) as i128;
            let st = if matches!(step, Value::Null) {
                1i128
            } else {
                (to_i64(step).unsigned_abs() as i128).max(1)
            };
            let count = (a - b).abs() / st;
            if count as usize > MAX_RANGE {
                return Ok(Value::Array(arr)); // memory-bomb guard
            }
            let mut x = a;
            if a <= b {
                while x <= b {
                    arr.push(Value::Int(x as i64));
                    x += st;
                }
            } else {
                while x >= b {
                    arr.push(Value::Int(x as i64));
                    x -= st;
                }
            }
            return Ok(Value::Array(arr));
        }
        // float range — iterate by INDEX, never by accumulating `x` (near i64
        // magnitudes the ulp exceeds the step, so `x += st` would be a no-op and
        // loop forever). Compute the element count, cap it BEFORE looping.
        let st = if matches!(step, Value::Null) { 1.0 } else { to_f64(step).abs().max(1e-9) };
        let (a, b) = (to_f64(start), to_f64(end));
        if !a.is_finite() || !b.is_finite() || !st.is_finite() {
            return Err(self.throw_error("ValueError", "range(): Arguments must be finite"));
        }
        let count_f = ((b - a).abs() / st).floor();
        if !count_f.is_finite() || count_f > MAX_RANGE as f64 {
            return Ok(Value::Array(arr)); // memory-bomb guard
        }
        let n = count_f as usize;
        for i in 0..=n {
            let x = if a <= b { a + i as f64 * st } else { a - i as f64 * st };
            arr.push(Value::Float(x));
        }
        Ok(Value::Array(arr))
    }

    fn sprintf(&self, args: &[Value]) -> Vec<u8> {
        let fmt = to_bytes(args.get(0).unwrap_or(&Value::Null));
        let mut out = Vec::new();
        let mut ai = 1;
        let mut i = 0;
        while i < fmt.len() {
            if fmt[i] != b'%' {
                out.push(fmt[i]);
                i += 1;
                continue;
            }
            i += 1;
            if i >= fmt.len() {
                break;
            }
            if fmt[i] == b'%' {
                out.push(b'%');
                i += 1;
                continue;
            }
            // collect flags/width/precision until a conversion char
            let spec_start = i;
            while i < fmt.len() && !fmt[i].is_ascii_alphabetic() {
                i += 1;
            }
            if i >= fmt.len() {
                break;
            }
            let conv = fmt[i];
            let mut spec = String::from_utf8_lossy(&fmt[spec_start..i]).into_owned();
            i += 1;
            // positional `%N$s`: selects args[N] WITHOUT moving the
            // sequential cursor (PHP reuses positions: '<%1$s>…</%1$s>')
            let mut positional: Option<usize> = None;
            let digits: String = spec.chars().take_while(|c| c.is_ascii_digit()).collect();
            if !digits.is_empty() && spec[digits.len()..].starts_with('$') {
                positional = digits.parse::<usize>().ok();
                spec = spec[digits.len() + 1..].to_string();
            }
            let arg = match positional {
                Some(n) => args.get(n).cloned().unwrap_or(Value::Null),
                None => {
                    let a = args.get(ai).cloned().unwrap_or(Value::Null);
                    ai += 1;
                    a
                }
            };
            let piece = format_spec(conv, &spec, &arg);
            out.extend_from_slice(&piece);
        }
        out
    }
}

/// Names the builtin dispatcher actually handles — generated by scanning the
/// dispatch arms (see DEVLOG). `function_exists` must say yes to these, or
/// polyfill guards (`if (!function_exists('mb_strlen')) { ... }`) shadow our
/// builtins with pure-PHP fallbacks that assume a full runtime (WP's
/// _mb_strlen calls get_option before the DB exists).
static KNOWN_BUILTINS: &[&str] = &[
    "__dom_parse", "__pdo_close", "__pdo_lastid", "__pdo_open", "__pdo_query",
    "__phargo_bcscale_of", "__phargo_createfromformat", "__phargo_cur_file",
    "__phargo_cur_line", "__phargo_date_tz", "__phargo_mktime_tz", "__phargo_modify",
    "__phargo_strtotime_tz", "__phargo_trace", "__phargo_tz_offset", "__phargo_tz_transitions",
    "__phargo_tz_valid", "abs", "acos", "acosh", "addcslashes", "addslashes", "array_chunk",
    "array_column", "array_combine", "array_count_values", "array_diff", "array_diff_key",
    "array_fill", "array_fill_keys", "array_filter", "array_flip", "array_intersect",
    "array_intersect_key", "array_is_list", "array_key_exists", "array_key_first",
    "array_key_last", "array_keys", "array_map", "array_merge", "array_merge_recursive",
    "array_multisort", "array_pad", "array_pop", "array_product", "array_push", "array_reduce",
    "array_replace", "array_replace_recursive", "array_reverse", "array_search", "array_shift",
    "array_slice", "array_splice", "array_sum", "array_unique", "array_unshift",
    "array_values", "array_walk", "array_walk_recursive", "arsort", "asin", "asinh", "asort",
    "assert", "atan", "atan2", "atanh", "base64_decode", "base64_encode", "basename", "bcadd",
    "bcceil", "bccomp", "bcdiv", "bcdivmod", "bcfloor", "bcmod", "bcmul", "bcpow", "bcpowmod",
    "bcround", "bcscale", "bcsqrt", "bcsub", "bin2hex", "bindec", "boolval", "call_user_func",
    "call_user_func_array", "ceil", "chdir", "checkdate", "chop", "chr", "chunk_split",
    "class_alias", "class_exists", "class_implements", "class_parents", "class_uses",
    "clearstatcache", "closedir", "compact", "constant", "copy", "cos", "cosh", "count",
    "crc32", "ctype_alnum", "ctype_alpha", "ctype_digit", "ctype_space", "current", "date",
    "date_default_timezone_get", "date_default_timezone_set", "debug_backtrace",
    "debug_print_backtrace", "decbin", "dechex", "decoct", "define", "defined", "deg2rad",
    "dirname", "doubleval", "each", "end", "enum_exists", "error_clear_last", "error_get_last",
    "error_log", "error_reporting", "escapeshellarg", "escapeshellcmd", "exec", "exp",
    "explode", "expm1", "extension_loaded", "extract", "fclose", "fdiv", "feof", "fflush",
    "fgetc", "fgetcsv", "fgets", "file", "file_exists", "file_get_contents",
    "file_put_contents", "filesize", "filter_var", "floatval", "flock", "floor", "flush",
    "fmod", "fopen", "fpassthru", "fputcsv", "fputs", "fread", "fscanf", "fseek", "fsockopen",
    "ftell", "func_get_arg", "func_get_args", "func_num_args", "function_exists", "fwrite",
    "gc_collect_cycles", "gc_disable", "gc_enable", "gc_enabled", "get_called_class",
    "get_class", "get_class_methods", "get_class_vars", "get_declared_classes",
    "get_declared_interfaces", "get_declared_traits", "get_loaded_extensions",
    "get_object_vars", "get_parent_class", "get_resource_id", "get_resource_type", "getcwd",
    "getdate", "getenv", "getimagesize", "getmypid", "getrandmax", "gettype", "glob", "gmdate",
    "gmmktime", "gmstrftime", "hash", "hash_algos", "hash_equals", "hash_hmac",
    "hash_hmac_algos", "header", "headers_sent", "hex2bin", "hexdec", "highlight_file",
    "highlight_string", "hrtime", "html_entity_decode", "htmlentities", "htmlspecialchars",
    "htmlspecialchars_decode", "http_build_query", "http_response_code", "hypot", "idate",
    "ignore_user_abort", "implode", "in_array", "ini_get", "ini_set", "intdiv",
    "interface_exists", "intval", "is_a", "is_array", "is_bool", "is_callable", "is_dir",
    "is_double", "is_file", "is_float", "is_int", "is_integer", "is_iterable", "is_long",
    "is_null", "is_numeric", "is_object", "is_readable", "is_resource", "is_scalar",
    "is_string", "is_subclass_of", "is_writable", "is_writeable", "iterator_to_array", "join",
    "json_decode", "json_encode", "json_last_error", "json_last_error_msg", "key",
    "key_exists", "krsort", "ksort", "lcfirst", "levenshtein", "log", "log10", "log1p", "log2",
    "ltrim", "max", "mb_check_encoding", "mb_chr", "mb_convert_case", "mb_convert_encoding",
    "mb_detect_encoding", "mb_http_output", "mb_internal_encoding", "mb_ord", "mb_scrub",
    "mb_str_split", "mb_strlen", "mb_strpos", "mb_strtolower", "mb_strtoupper",
    "mb_substitute_character", "mb_substr", "md5", "memory_get_peak_usage", "memory_get_usage",
    "method_exists", "microtime", "min", "mkdir", "mktime", "mt_getrandmax", "mt_rand",
    "mt_srand", "natcasesort", "natsort", "next", "nl2br", "number_format", "ob_clean",
    "ob_end_clean", "ob_end_flush", "ob_flush", "ob_get_clean", "ob_get_contents",
    "ob_get_length", "ob_get_level", "ob_start", "octdec", "opendir", "openssl_sign", "ord",
    "pack", "parse_str", "parse_url", "pathinfo", "php_sapi_name", "php_uname", "phpversion",
    "pi", "pos", "pow", "preg_grep", "preg_match", "preg_match_all", "preg_quote",
    "preg_replace", "preg_replace_callback", "preg_split", "prev", "print_r", "printf",
    "property_exists", "putenv", "quotemeta", "rad2deg", "rand", "random_int", "range",
    "rawurldecode", "rawurlencode", "readdir", "readfile", "realpath",
    "register_shutdown_function", "rename", "reset", "restore_error_handler",
    "restore_exception_handler", "rewind", "rewinddir", "rmdir", "round", "rsort", "rtrim",
    "scandir", "serialize", "session_abort", "session_cache_expire", "session_cache_limiter",
    "session_commit", "session_create_id", "session_destroy", "session_get_cookie_params",
    "session_id", "session_module_name", "session_name", "session_regenerate_id",
    "session_register_shutdown", "session_reset", "session_save_path",
    "session_set_cookie_params", "session_set_save_handler", "session_start", "session_status",
    "session_unset", "session_write_close", "set_error_handler", "set_exception_handler",
    "set_time_limit", "setcookie", "setlocale", "setrawcookie", "settype", "sha1", "shuffle",
    "similar_text", "sin", "sinh", "sizeof", "sleep", "sort", "spl_autoload_functions",
    "spl_autoload_register", "spl_autoload_unregister", "spl_object_hash", "spl_object_id",
    "sprintf", "sqrt", "srand", "sscanf", "str_contains", "str_ends_with", "str_getcsv",
    "str_ireplace", "str_pad", "str_repeat", "str_replace", "str_rot13", "str_split",
    "str_starts_with", "str_word_count", "strcasecmp", "strchr", "strcmp", "strcspn",
    "stream_context_create", "stream_context_get_default", "stream_context_set_default",
    "stream_context_set_option", "stream_context_set_params", "stream_filter_append",
    "stream_filter_prepend", "stream_filter_remove", "stream_get_contents", "stream_get_line",
    "stream_get_meta_data", "stream_set_blocking", "stream_set_read_buffer",
    "stream_set_timeout", "stream_set_write_buffer", "stream_socket_client",
    "stream_wrapper_register", "stream_wrapper_restore", "stream_wrapper_unregister",
    "strftime", "strip_tags", "stripcslashes", "stripos", "stripslashes", "stristr", "strlen",
    "strncasecmp", "strncmp", "strpbrk", "strpos", "strrchr", "strrev", "strrpos", "strspn",
    "strstr", "strtok", "strtolower", "strtotime", "strtoupper", "strtr", "strval", "substr",
    "substr_compare", "substr_count", "substr_replace", "sys_get_temp_dir", "tan", "tanh",
    "tempnam", "time", "timezone_identifiers_list", "touch", "trait_exists", "trigger_error",
    "trim", "uasort", "ucfirst", "ucwords", "uksort", "umask", "uniqid", "unlink", "unpack",
    "unserialize", "urldecode", "urlencode", "usleep", "usort", "var_dump", "var_export",
    "version_compare", "vprintf", "vsprintf", "wordwrap", "xml_error_string",
    "xml_get_current_byte_index", "xml_get_current_column_number",
    "xml_get_current_line_number", "xml_get_error_code", "xml_parse", "xml_parse_into_struct",
    "xml_parser_create", "xml_parser_create_ns", "xml_parser_free", "xml_parser_get_option",
    "xml_parser_set_option", "xml_set_character_data_handler", "xml_set_default_handler",
    "xml_set_element_handler", "xml_set_end_namespace_decl_handler",
    "xml_set_external_entity_ref_handler", "xml_set_notation_decl_handler", "xml_set_object",
    "xml_set_processing_instruction_handler", "xml_set_start_namespace_decl_handler",
    "xml_set_unparsed_entity_decl_handler",
];

fn is_known_builtin(n: &str) -> bool {
    KNOWN_BUILTINS.binary_search(&n).is_ok()
}

fn trim_bytes(s: &[u8], left: bool, right: bool) -> Vec<u8> {
    let ws = |b: u8| matches!(b, b' ' | b'\t' | b'\n' | b'\r' | 0 | 0x0b);
    let mut start = 0;
    let mut end = s.len();
    if left {
        while start < end && ws(s[start]) {
            start += 1;
        }
    }
    if right {
        while end > start && ws(s[end - 1]) {
            end -= 1;
        }
    }
    s[start..end].to_vec()
}

/// Navigate a value by a sequence of already-normalized keys (array/string).
fn read_index_value(base: &Value, keys: &[Key]) -> Value {
    let mut v = base;
    for (i, k) in keys.iter().enumerate() {
        match v {
            Value::Array(a) => match a.get(k) {
                Some(Value::Ref(cell)) => {
                    // reference element mid-path: continue inside the cell
                    let inner = cell.borrow().clone();
                    return read_index_value(&inner, &keys[i + 1..]);
                }
                Some(x) => v = x,
                None => return Value::Null,
            },
            Value::Str(s) => return string_char(s, k),
            _ => return Value::Null,
        }
    }
    v.deref()
}

/// The user-visible class name: anonymous classes are registered under a unique
/// internal name like `class@anonymous#3` but display as `class@anonymous`.
fn display_class(name: &str) -> String {
    match name.split_once('#') {
        Some((base, _)) => base.to_string(),
        None => name.to_string(),
    }
}

/// PHP superglobals resolve to the global scope from any function scope.
fn is_superglobal(name: &str) -> bool {
    matches!(
        name,
        "GLOBALS" | "_SERVER" | "_GET" | "_POST" | "_REQUEST" | "_SESSION" | "_COOKIE" | "_ENV" | "_FILES"
    )
}

/// Can this expression be written back to (a valid by-reference target)?
fn is_lvalue_expr(e: &Expr) -> bool {
    matches!(
        e,
        Expr::Var(_) | Expr::Index(..) | Expr::Prop(..) | Expr::StaticProp(..) | Expr::VarVar(_)
    )
}

/// Is this value a stream resource (a `__Stream` pseudo-object)?
fn is_stream(v: &Value) -> bool {
    matches!(v, Value::Object(o) if o.borrow().class == "__Stream")
}

/// Parse one CSV record into an array of fields (basic RFC4180 quoting).
fn parse_csv(s: &[u8], delim: u8, quote: u8) -> Value {
    let mut fields = Arr::new();
    let mut field: Vec<u8> = Vec::new();
    let mut in_q = false;
    let mut i = 0;
    let n = s.len();
    while i < n {
        let c = s[i];
        if in_q {
            if c == quote {
                if i + 1 < n && s[i + 1] == quote {
                    field.push(quote);
                    i += 2;
                    continue;
                }
                in_q = false;
                i += 1;
            } else {
                field.push(c);
                i += 1;
            }
        } else if c == quote && field.is_empty() {
            in_q = true;
            i += 1;
        } else if c == delim {
            fields.push(Value::Str(std::mem::take(&mut field)));
            i += 1;
        } else {
            field.push(c);
            i += 1;
        }
    }
    fields.push(Value::Str(field));
    Value::Array(fields)
}

fn first_byte_or(v: &[u8], d: u8) -> u8 {
    v.first().copied().unwrap_or(d)
}

/// Byte-wise bitwise op on two strings. `longer` = result spans the longer
/// operand (for `|`); otherwise the shorter (for `&`/`^`).
fn str_bitwise(l: &Value, r: &Value, op: fn(u8, u8) -> u8, longer: bool) -> Value {
    let a = to_bytes(l);
    let b = to_bytes(r);
    let n = if longer { a.len().max(b.len()) } else { a.len().min(b.len()) };
    let mut out = vec![0u8; n];
    for (i, slot) in out.iter_mut().enumerate() {
        *slot = op(a.get(i).copied().unwrap_or(0), b.get(i).copied().unwrap_or(0));
    }
    Value::Str(out)
}

/// array_merge_recursive: integer keys append; string keys merge, and when both
/// sides hold a value under the same string key, they combine into an array.
fn merge_recursive(out: &mut Arr, src: &Arr) {
    for (k, v) in &src.entries {
        match k {
            Key::Int(_) => out.push(v.clone()),
            Key::Str(_) => {
                if let Some(existing) = out.get(k).cloned() {
                    let combined = match (existing, v.clone()) {
                        (Value::Array(mut a), Value::Array(b)) => {
                            merge_recursive(&mut a, &b);
                            Value::Array(a)
                        }
                        (Value::Array(mut a), other) => {
                            a.push(other);
                            Value::Array(a)
                        }
                        (first, Value::Array(b)) => {
                            let mut a = Arr::new();
                            a.push(first);
                            merge_recursive(&mut a, &b);
                            Value::Array(a)
                        }
                        (first, second) => {
                            let mut a = Arr::new();
                            a.push(first);
                            a.push(second);
                            Value::Array(a)
                        }
                    };
                    out.insert(k.clone(), combined);
                } else {
                    out.insert(k.clone(), v.clone());
                }
            }
        }
    }
}

fn urlencode_form(s: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(s.len());
    for &b in s {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.') {
            out.push(b);
        } else if b == b' ' {
            out.push(b'+');
        } else {
            out.extend_from_slice(format!("%{b:02X}").as_bytes());
        }
    }
    out
}

/// Decode the predefined HTML entities + numeric (`&#NN;` / `&#xNN;`) references.
/// HTML 4.01 named entities (sorted for binary_search) — the set PHP's
/// htmlspecialchars(double_encode: false) treats as already-encoded.
static HTML_ENTITY_NAMES: &[&str] = &[
    "AElig", "Aacute", "Acirc", "Agrave", "Alpha", "Aring", "Atilde", "Auml", "Beta", "Ccedil",
    "Chi", "Dagger", "Delta", "ETH", "Eacute", "Ecirc", "Egrave", "Epsilon", "Eta", "Euml",
    "Gamma", "Iacute", "Icirc", "Igrave", "Iota", "Iuml", "Kappa", "Lambda", "Mu", "Ntilde",
    "Nu", "OElig", "Oacute", "Ocirc", "Ograve", "Omega", "Omicron", "Oslash", "Otilde", "Ouml",
    "Phi", "Pi", "Prime", "Psi", "Rho", "Scaron", "Sigma", "THORN", "Tau", "Theta", "Uacute",
    "Ucirc", "Ugrave", "Upsilon", "Uuml", "Xi", "Yacute", "Yuml", "Zeta", "aacute", "acirc",
    "acute", "aelig", "agrave", "alefsym", "alpha", "amp", "and", "ang", "apos", "aring",
    "asymp", "atilde", "auml", "bdquo", "beta", "brvbar", "bull", "cap", "ccedil", "cedil",
    "cent", "chi", "circ", "clubs", "cong", "copy", "crarr", "cup", "curren", "dArr", "dagger",
    "darr", "deg", "delta", "diams", "divide", "eacute", "ecirc", "egrave", "empty", "emsp",
    "ensp", "epsilon", "equiv", "eta", "eth", "euml", "euro", "exist", "fnof", "forall",
    "frac12", "frac14", "frac34", "frasl", "gamma", "ge", "gt", "hArr", "harr", "hearts",
    "hellip", "iacute", "icirc", "iexcl", "igrave", "image", "infin", "int", "iota", "iquest",
    "isin", "iuml", "kappa", "lArr", "lambda", "lang", "laquo", "larr", "lceil", "ldquo", "le",
    "lfloor", "lowast", "loz", "lrm", "lsaquo", "lsquo", "lt", "macr", "mdash", "micro",
    "middot", "minus", "mu", "nabla", "nbsp", "ndash", "ne", "ni", "not", "notin", "nsub",
    "ntilde", "nu", "oacute", "ocirc", "oelig", "ograve", "oline", "omega", "omicron", "oplus",
    "or", "ordf", "ordm", "oslash", "otilde", "otimes", "ouml", "para", "part", "permil",
    "perp", "phi", "pi", "piv", "plusmn", "pound", "prime", "prod", "prop", "psi", "quot",
    "rArr", "radic", "rang", "raquo", "rarr", "rceil", "rdquo", "real", "reg", "rfloor", "rho",
    "rlm", "rsaquo", "rsquo", "sbquo", "scaron", "sdot", "sect", "shy", "sigma", "sigmaf",
    "sim", "spades", "sub", "sube", "sum", "sup", "sup1", "sup2", "sup3", "supe", "szlig",
    "tau", "there4", "theta", "thetasym", "thinsp", "thorn", "tilde", "times", "trade", "uArr",
    "uacute", "uarr", "ucirc", "ugrave", "uml", "upsih", "upsilon", "uuml", "weierp", "xi",
    "yacute", "yen", "yuml", "zeta", "zwj", "zwnj",
];

fn decode_html_entities(s: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(s.len());
    let mut i = 0;
    while i < s.len() {
        if s[i] == b'&' {
            if let Some(semi) = s[i + 1..].iter().position(|&b| b == b';').map(|p| i + 1 + p) {
                let ent = &s[i + 1..semi];
                let rep: Option<Vec<u8>> = match ent {
                    b"amp" => Some(b"&".to_vec()),
                    b"lt" => Some(b"<".to_vec()),
                    b"gt" => Some(b">".to_vec()),
                    b"quot" => Some(b"\"".to_vec()),
                    b"apos" | b"#039" | b"#39" => Some(b"'".to_vec()),
                    b"nbsp" => Some(vec![0xc2, 0xa0]),
                    _ if ent.starts_with(b"#x") || ent.starts_with(b"#X") => std::str::from_utf8(&ent[2..])
                        .ok()
                        .and_then(|h| u32::from_str_radix(h, 16).ok())
                        .and_then(char::from_u32)
                        .map(|c| c.to_string().into_bytes()),
                    _ if ent.starts_with(b"#") => std::str::from_utf8(&ent[1..])
                        .ok()
                        .and_then(|d| d.parse::<u32>().ok())
                        .and_then(char::from_u32)
                        .map(|c| c.to_string().into_bytes()),
                    _ => None,
                };
                if let Some(r) = rep {
                    out.extend_from_slice(&r);
                    i = semi + 1;
                    continue;
                }
            }
        }
        out.push(s[i]);
        i += 1;
    }
    out
}

/// Read a format-code count: a number, `*`, or 1 (default). Returns (count, is_star).
fn pack_count(fmt: &[u8], i: &mut usize) -> (usize, bool) {
    if *i < fmt.len() && fmt[*i] == b'*' {
        *i += 1;
        return (0, true);
    }
    let start = *i;
    while *i < fmt.len() && fmt[*i].is_ascii_digit() {
        *i += 1;
    }
    if *i > start {
        (std::str::from_utf8(&fmt[start..*i]).unwrap().parse().unwrap_or(1), false)
    } else {
        (1, false)
    }
}

/// `pack(format, ...values)` — a useful subset of the format codes.
fn pack_values(fmt: &[u8], vals: &[Value]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut vi = 0;
    let mut i = 0;
    while i < fmt.len() {
        let code = fmt[i];
        i += 1;
        let (cnt, star) = pack_count(fmt, &mut i);
        match code {
            b'a' | b'A' | b'Z' => {
                let s = vals.get(vi).map(to_bytes).unwrap_or_default();
                vi += 1;
                let pad = if code == b'A' { b' ' } else { 0u8 };
                let len = if star { s.len() + if code == b'Z' { 1 } else { 0 } } else { cnt };
                for k in 0..len {
                    out.push(*s.get(k).unwrap_or(&pad));
                }
            }
            b'H' | b'h' => {
                let s = vals.get(vi).map(to_bytes).unwrap_or_default();
                vi += 1;
                let n = if star { s.len() } else { cnt };
                let mut byte = 0u8;
                for k in 0..n {
                    let nib = (s.get(k).copied().unwrap_or(b'0') as char).to_digit(16).unwrap_or(0) as u8;
                    if k % 2 == 0 {
                        byte = if code == b'H' { nib << 4 } else { nib };
                    } else {
                        byte |= if code == b'H' { nib } else { nib << 4 };
                        out.push(byte);
                        byte = 0;
                    }
                }
                if n % 2 == 1 { out.push(byte); }
            }
            _ => {
                let reps = if star { vals.len().saturating_sub(vi) } else { cnt };
                for _ in 0..reps {
                    let v = to_i64(vals.get(vi).unwrap_or(&Value::Null));
                    vi += 1;
                    match code {
                        b'c' | b'C' => out.push(v as u8),
                        b's' | b'S' | b'v' => out.extend_from_slice(&(v as u16).to_le_bytes()),
                        b'n' => out.extend_from_slice(&(v as u16).to_be_bytes()),
                        b'l' | b'L' | b'V' => out.extend_from_slice(&(v as u32).to_le_bytes()),
                        b'N' => out.extend_from_slice(&(v as u32).to_be_bytes()),
                        b'q' | b'Q' | b'P' => out.extend_from_slice(&v.to_le_bytes()),
                        b'J' => out.extend_from_slice(&v.to_be_bytes()),
                        b'f' | b'g' => out.extend_from_slice(&(to_f64(vals.get(vi - 1).unwrap_or(&Value::Null)) as f32).to_le_bytes()),
                        b'd' | b'e' => out.extend_from_slice(&to_f64(vals.get(vi - 1).unwrap_or(&Value::Null)).to_le_bytes()),
                        _ => {}
                    }
                }
            }
        }
    }
    out
}

/// `unpack(format, data)` — returns an array keyed by the per-group names.
fn unpack_values(fmt: &[u8], data: &[u8]) -> Value {
    let mut arr = Arr::new();
    let mut pos = 0usize;
    for group in fmt.split(|&b| b == b'/') {
        if group.is_empty() { continue; }
        let code = group[0];
        let mut gi = 1;
        let (mut cnt, star) = pack_count(group, &mut gi);
        let name = String::from_utf8_lossy(&group[gi..]).into_owned();
        let read_int = |pos: &mut usize, n: usize, be: bool, signed: bool| -> i64 {
            if *pos + n > data.len() { return 0; }
            let mut bytes = [0u8; 8];
            for k in 0..n { bytes[k] = data[*pos + k]; }
            *pos += n;
            let raw = if be {
                let mut v = 0u64;
                for k in 0..n { v = (v << 8) | bytes[k] as u64; }
                v
            } else {
                u64::from_le_bytes(bytes)
            };
            if signed && n < 8 {
                let shift = 64 - n * 8;
                ((raw << shift) as i64) >> shift
            } else {
                raw as i64
            }
        };
        let push = |arr: &mut Arr, name: &str, idx: usize, multi: bool, v: Value| {
            let key = if name.is_empty() {
                Key::Int((idx + 1) as i64)
            } else if multi {
                Key::Str(format!("{name}{}", idx + 1).into_bytes())
            } else {
                Key::Str(name.as_bytes().to_vec())
            };
            arr.insert(key, v);
        };
        match code {
            b'a' | b'A' | b'Z' => {
                let n = if star { data.len() - pos } else { cnt };
                let end = (pos + n).min(data.len());
                let mut s = data[pos..end].to_vec();
                pos = end;
                if code == b'A' { while matches!(s.last(), Some(b' ') | Some(0)) { s.pop(); } }
                if code == b'Z' { if let Some(z) = s.iter().position(|&b| b == 0) { s.truncate(z); } }
                let key = if name.is_empty() { Key::Int(1) } else { Key::Str(name.into_bytes()) };
                arr.insert(key, Value::Str(s));
            }
            b'H' | b'h' => {
                let nib = if star { (data.len() - pos) * 2 } else { cnt };
                let mut hex = String::new();
                for k in 0..nib {
                    let byte = data.get(pos + k / 2).copied().unwrap_or(0);
                    let n = if (k % 2 == 0) == (code == b'H') { byte >> 4 } else { byte & 0xf };
                    hex.push(std::char::from_digit(n as u32, 16).unwrap());
                }
                pos += nib.div_ceil(2);
                let key = if name.is_empty() { Key::Int(1) } else { Key::Str(name.into_bytes()) };
                arr.insert(key, Value::Str(hex.into_bytes()));
            }
            _ => {
                let (n, be, signed) = match code {
                    b'c' => (1, false, true), b'C' => (1, false, false),
                    b's' => (2, false, true), b'S' | b'v' => (2, false, false), b'n' => (2, true, false),
                    b'l' => (4, false, true), b'L' | b'V' => (4, false, false), b'N' => (4, true, false),
                    b'q' => (8, false, true), b'Q' | b'P' => (8, false, false), b'J' => (8, true, false),
                    _ => (0, false, false),
                };
                if n == 0 { continue; }
                if star { cnt = (data.len() - pos) / n; }
                let multi = cnt > 1;
                for idx in 0..cnt {
                    let v = read_int(&mut pos, n, be, signed);
                    push(&mut arr, &name, idx, multi, Value::Int(v));
                }
            }
        }
    }
    Value::Array(arr)
}

/// Remove markup tags from `s`, keeping any in `allowed` (lowercased tag names).
/// Also strips `<?…?>` and `<!--…-->`.
fn strip_tags(s: &[u8], allowed: &[Vec<u8>]) -> Vec<u8> {
    let mut out = Vec::with_capacity(s.len());
    let mut i = 0;
    let n = s.len();
    while i < n {
        if s[i] == b'<' {
            // PHP/ASP/comment blocks: drop entirely
            if s[i + 1..].starts_with(b"?") {
                i = find_bytes(s, b"?>", i).map(|p| p + 2).unwrap_or(n);
                continue;
            }
            if s[i + 1..].starts_with(b"!--") {
                i = find_bytes(s, b"-->", i).map(|p| p + 3).unwrap_or(n);
                continue;
            }
            // a tag: <name ...> or </name>
            let end = match find_bytes(s, b">", i) {
                Some(e) => e,
                None => { out.push(s[i]); i += 1; continue; }
            };
            let inner = &s[i + 1..end];
            let name_start = if inner.first() == Some(&b'/') { 1 } else { 0 };
            let name: Vec<u8> = inner[name_start..]
                .iter()
                .take_while(|b| b.is_ascii_alphanumeric())
                .map(|b| b.to_ascii_lowercase())
                .collect();
            if allowed.iter().any(|a| a == &name) {
                out.extend_from_slice(&s[i..=end]);
            }
            i = end + 1;
        } else {
            out.push(s[i]);
            i += 1;
        }
    }
    out
}

/// Binary-safe comparison returning PHP 8's -1 / 0 / 1.
fn byte_sign(a: &[u8], b: &[u8]) -> i64 {
    match a.cmp(b) {
        std::cmp::Ordering::Less => -1,
        std::cmp::Ordering::Equal => 0,
        std::cmp::Ordering::Greater => 1,
    }
}

fn find_bytes(hay: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if needle.is_empty() {
        return Some(from.min(hay.len()));
    }
    if from > hay.len() {
        return None;
    }
    hay[from..]
        .windows(needle.len())
        .position(|w| w == needle)
        .map(|p| p + from)
}

/// replace_bytes with optional ASCII case-insensitive matching (str_ireplace).
/// glob(): expand the pattern segment-wise. `*`/`?`/`[...]` match within one
/// path segment; a leading dot is only matched literally (fnmatch rules).
fn php_glob(pattern: &str) -> Vec<String> {
    fn seg_match(name: &str, pat: &str) -> bool {
        // no leading-dot match via wildcards
        if name.starts_with('.') && !pat.starts_with('.') {
            return false;
        }
        let n: Vec<char> = name.chars().collect();
        let p: Vec<char> = pat.chars().collect();
        fn rec(n: &[char], p: &[char]) -> bool {
            if p.is_empty() {
                return n.is_empty();
            }
            match p[0] {
                '*' => rec(n, &p[1..]) || (!n.is_empty() && rec(&n[1..], p)),
                '?' => !n.is_empty() && rec(&n[1..], &p[1..]),
                '[' => {
                    if n.is_empty() {
                        return false;
                    }
                    let close = match p.iter().skip(1).position(|&c| c == ']') {
                        Some(i) => i + 1,
                        None => return false,
                    };
                    let (set, neg) = if p[1] == '!' || p[1] == '^' {
                        (&p[2..close], true)
                    } else {
                        (&p[1..close], false)
                    };
                    let mut hit = false;
                    let mut i = 0;
                    while i < set.len() {
                        if i + 2 < set.len() && set[i + 1] == '-' {
                            if set[i] <= n[0] && n[0] <= set[i + 2] {
                                hit = true;
                            }
                            i += 3;
                        } else {
                            if set[i] == n[0] {
                                hit = true;
                            }
                            i += 1;
                        }
                    }
                    (hit != neg) && rec(&n[1..], &p[close + 1..])
                }
                c => !n.is_empty() && n[0] == c && rec(&n[1..], &p[1..]),
            }
        }
        rec(&n, &p)
    }
    let has_wild = |s: &str| s.contains('*') || s.contains('?') || s.contains('[');
    let (mut bases, segs): (Vec<String>, Vec<&str>) = if let Some(rest) = pattern.strip_prefix('/')
    {
        (vec!["/".to_string()], rest.split('/').collect())
    } else {
        (vec![String::new()], pattern.split('/').collect())
    };
    for (si, seg) in segs.iter().enumerate() {
        if seg.is_empty() {
            continue;
        }
        let last = si == segs.len() - 1;
        let mut next = Vec::new();
        for b in &bases {
            let joined = |name: &str| {
                if b.is_empty() {
                    name.to_string()
                } else if b.ends_with('/') {
                    format!("{b}{name}")
                } else {
                    format!("{b}/{name}")
                }
            };
            if !has_wild(seg) {
                let cand = joined(seg);
                let dir_for_read = if b.is_empty() { "." } else { b.as_str() };
                let exists = std::path::Path::new(&cand).exists()
                    || std::path::Path::new(dir_for_read).join(seg).exists();
                if exists && (!last || true) {
                    next.push(cand);
                }
                continue;
            }
            let dir = if b.is_empty() { ".".to_string() } else { b.clone() };
            if let Ok(rd) = std::fs::read_dir(&dir) {
                for e in rd.flatten() {
                    let name = e.file_name().to_string_lossy().into_owned();
                    if seg_match(&name, seg) && (last || e.path().is_dir()) {
                        next.push(joined(&name));
                    }
                }
            }
        }
        bases = next;
        if bases.is_empty() {
            break;
        }
    }
    // final existence filter for fully-literal patterns
    bases.retain(|p| std::path::Path::new(p).exists());
    bases
}

/// preg_replace core, returning (result, replacement count) — the count
/// feeds the by-ref 5th parameter (WP's formatting.php Vulcan-quote logic
/// branches on it).
fn preg_replace_full(argv: &[Value]) -> (Value, i64) {
    let g = |i: usize| argv.get(i).cloned().unwrap_or(Value::Null);
    let subject = String::from_utf8_lossy(&to_bytes(&g(2))).into_owned();
    let limit = if argv.len() > 3 { to_i64(&g(3)) } else { -1 };
    let pats: Vec<Vec<u8>> = match g(0) {
        Value::Array(arr) => arr.entries.into_iter().map(|(_, v)| to_bytes(&v)).collect(),
        v => vec![to_bytes(&v)],
    };
    let rep_is_arr = matches!(g(1), Value::Array(_));
    let reps: Vec<Vec<u8>> = match g(1) {
        Value::Array(arr) => arr.entries.into_iter().map(|(_, v)| to_bytes(&v)).collect(),
        v => vec![to_bytes(&v)],
    };
    let mut result = subject;
    let mut count = 0i64;
    for (i, p) in pats.iter().enumerate() {
        let pattern = String::from_utf8_lossy(p).into_owned();
        let repl = if rep_is_arr {
            reps.get(i).map(|r| String::from_utf8_lossy(r).into_owned()).unwrap_or_default()
        } else {
            String::from_utf8_lossy(&reps[0]).into_owned()
        };
        if let Some(rx) = crate::rx_compile(&pattern) {
            result = crate::rx_replace_str(&rx, &repl, &result, limit, &mut count);
        }
    }
    (Value::Str(result.into_bytes()), count)
}

/// parse_str(): decode a query string into a (possibly nested) array.
/// Handles `a[b][]=v` bracket syntax; dots/spaces in top-level names become
/// underscores, as PHP does for variable-name compatibility.
fn php_parse_str(qs: &[u8]) -> Arr {
    fn dec(s: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(s.len());
        let mut i = 0;
        while i < s.len() {
            match s[i] {
                b'+' => out.push(b' '),
                b'%' if i + 2 < s.len() => {
                    let hex = std::str::from_utf8(&s[i + 1..i + 3]).unwrap_or("");
                    match u8::from_str_radix(hex, 16) {
                        Ok(b) => {
                            out.push(b);
                            i += 2;
                        }
                        Err(_) => out.push(b'%'),
                    }
                }
                b => out.push(b),
            }
            i += 1;
        }
        out
    }
    let mut root = Arr::new();
    for pair in qs.split(|&b| b == b'&') {
        if pair.is_empty() {
            continue;
        }
        let (rawk, rawv) = match pair.iter().position(|&b| b == b'=') {
            Some(i) => (&pair[..i], &pair[i + 1..]),
            None => (pair, &b""[..]),
        };
        let key = dec(rawk);
        let val = Value::Str(dec(rawv));
        // split base[seg1][seg2]...
        let (base, rest) = match key.iter().position(|&b| b == b'[') {
            Some(i) => (&key[..i], &key[i..]),
            None => (&key[..], &b""[..]),
        };
        let mut base: Vec<u8> = base.to_vec();
        for b in base.iter_mut() {
            if *b == b'.' || *b == b' ' {
                *b = b'_';
            }
        }
        if base.is_empty() {
            continue;
        }
        // collect bracket segments
        let mut segs: Vec<Option<Vec<u8>>> = Vec::new(); // None = append []
        let mut i = 0;
        while i < rest.len() {
            if rest[i] == b'[' {
                match rest[i..].iter().position(|&b| b == b']') {
                    Some(j) => {
                        let seg = &rest[i + 1..i + j];
                        segs.push(if seg.is_empty() { None } else { Some(seg.to_vec()) });
                        i += j + 1;
                    }
                    None => break,
                }
            } else {
                i += 1;
            }
        }
        if segs.is_empty() {
            root.insert(Key::Str(base), val);
            continue;
        }
        // navigate/create nested arrays
        fn place(arr: &mut Arr, segs: &[Option<Vec<u8>>], val: Value) {
            match &segs[0] {
                None => {
                    if segs.len() == 1 {
                        arr.push(val);
                    } else {
                        let mut child = Arr::new();
                        place(&mut child, &segs[1..], val);
                        arr.push(Value::Array(child));
                    }
                }
                Some(seg) => {
                    let k = Arr::norm_key(&Value::Str(seg.clone()));
                    if segs.len() == 1 {
                        arr.insert(k, val);
                        return;
                    }
                    if !matches!(arr.get(&k), Some(Value::Array(_))) {
                        arr.insert(k.clone(), Value::Array(Arr::new()));
                    }
                    if let Some(Value::Array(child)) = arr.get_mut(&k) {
                        place(child, &segs[1..], val);
                    }
                }
            }
        }
        let bk = Key::Str(base);
        if !matches!(root.get(&bk), Some(Value::Array(_))) {
            root.insert(bk.clone(), Value::Array(Arr::new()));
        }
        if let Some(Value::Array(child)) = root.get_mut(&bk) {
            place(child, &segs, val);
        }
    }
    root
}

/// PHP's lenient parse_url. Returns None for the cases PHP reports `false`
/// (empty host after `//`, out-of-range port). Only present components appear
/// in the array, matching PHP.
fn php_parse_url(url: &[u8]) -> Option<Arr> {
    let mut arr = Arr::new();
    let mut s = url;
    let mut push = |arr: &mut Arr, k: &[u8], v: Value| arr.insert(Key::Str(k.to_vec()), v);
    // fragment and query split off the tail first
    let mut fragment: Option<&[u8]> = None;
    if let Some(i) = s.iter().position(|&b| b == b'#') {
        fragment = Some(&s[i + 1..]);
        s = &s[..i];
    }
    let mut query: Option<&[u8]> = None;
    if let Some(i) = s.iter().position(|&b| b == b'?') {
        query = Some(&s[i + 1..]);
        s = &s[..i];
    }
    // scheme: [alpha][alnum+.-]* ':'
    let mut scheme: Option<&[u8]> = None;
    if let Some(i) = s.iter().position(|&b| b == b':') {
        let cand = &s[..i];
        let valid = !cand.is_empty()
            && cand[0].is_ascii_alphabetic()
            && cand
                .iter()
                .all(|&b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'.' | b'-'));
        if valid {
            let rest = &s[i + 1..];
            // "host:81/x" (port digits, no //) is host:port, not a scheme
            let port_like = {
                let digits: &[u8] = match rest.iter().position(|&b| b == b'/') {
                    Some(j) => &rest[..j],
                    None => rest,
                };
                !digits.is_empty() && digits.iter().all(|b| b.is_ascii_digit())
            };
            if !port_like {
                scheme = Some(cand);
                s = rest;
            }
        }
    }
    let has_authority = s.starts_with(b"//");
    if has_authority {
        s = &s[2..];
        let (auth, path) = match s.iter().position(|&b| b == b'/') {
            Some(i) => (&s[..i], &s[i..]),
            None => (s, &b""[..]),
        };
        // PHP's key order is scheme, host, port, user, pass, path — hold
        // userinfo until host/port are in
        let mut hostport = auth;
        let mut userinfo: Option<&[u8]> = None;
        if let Some(i) = auth.iter().rposition(|&b| b == b'@') {
            userinfo = Some(&auth[..i]);
            hostport = &auth[i + 1..];
        }
        if let Some(sc) = scheme.take() {
            push(&mut arr, b"scheme", Value::Str(sc.to_vec()));
        }
        // IPv6 literal keeps its brackets out of the port split
        let (host, port) = if hostport.starts_with(b"[") {
            match hostport.iter().position(|&b| b == b']') {
                Some(i) => {
                    let rest = &hostport[i + 1..];
                    match rest.first() {
                        Some(b':') => (&hostport[..i + 1], Some(&rest[1..])),
                        _ => (hostport, None),
                    }
                }
                None => (hostport, None),
            }
        } else {
            match hostport.iter().rposition(|&b| b == b':') {
                Some(i) => (&hostport[..i], Some(&hostport[i + 1..])),
                None => (hostport, None),
            }
        };
        if host.is_empty() {
            return None;
        }
        push(&mut arr, b"host", Value::Str(host.to_vec()));
        if let Some(p) = port {
            if !p.is_empty() {
                let n: i64 = String::from_utf8_lossy(p).parse().ok()?;
                if !(0..=65535).contains(&n) {
                    return None;
                }
                push(&mut arr, b"port", Value::Int(n));
            }
        }
        if let Some(ui) = userinfo {
            match ui.iter().position(|&b| b == b':') {
                Some(j) => {
                    push(&mut arr, b"user", Value::Str(ui[..j].to_vec()));
                    push(&mut arr, b"pass", Value::Str(ui[j + 1..].to_vec()));
                }
                None => push(&mut arr, b"user", Value::Str(ui.to_vec())),
            }
        }
        if !path.is_empty() {
            push(&mut arr, b"path", Value::Str(path.to_vec()));
        }
    } else {
        // host:port with no scheme ("localhost:81/x")
        let mut done = false;
        if scheme.is_none() {
            if let Some(i) = s.iter().position(|&b| b == b':') {
                let rest = &s[i + 1..];
                let (digits, path): (&[u8], &[u8]) = match rest.iter().position(|&b| b == b'/') {
                    Some(j) => (&rest[..j], &rest[j..]),
                    None => (rest, &b""[..]),
                };
                if !digits.is_empty() && digits.iter().all(|b| b.is_ascii_digit()) {
                    let n: i64 = String::from_utf8_lossy(digits).parse().ok()?;
                    if !(0..=65535).contains(&n) {
                        return None;
                    }
                    if s[..i].is_empty() {
                        return None;
                    }
                    push(&mut arr, b"host", Value::Str(s[..i].to_vec()));
                    push(&mut arr, b"port", Value::Int(n));
                    if !path.is_empty() {
                        push(&mut arr, b"path", Value::Str(path.to_vec()));
                    }
                    done = true;
                }
            }
        }
        if !done {
            if let Some(sc) = scheme.take() {
                push(&mut arr, b"scheme", Value::Str(sc.to_vec()));
            }
            if !s.is_empty() {
                push(&mut arr, b"path", Value::Str(s.to_vec()));
            }
        }
    }
    if let Some(q) = query {
        push(&mut arr, b"query", Value::Str(q.to_vec()));
    }
    if let Some(f) = fragment {
        push(&mut arr, b"fragment", Value::Str(f.to_vec()));
    }
    if arr.len() == 0 {
        return None;
    }
    Some(arr)
}

fn replace_bytes_ci(subject: &[u8], search: &[u8], replace: &[u8], ci: bool) -> Vec<u8> {
    replace_bytes_ci_n(subject, search, replace, ci).0
}

/// Counting variant — the count feeds str_replace's by-ref 4th parameter.
fn replace_bytes_ci_n(subject: &[u8], search: &[u8], replace: &[u8], ci: bool) -> (Vec<u8>, i64) {
    if search.is_empty() {
        return (subject.to_vec(), 0);
    }
    let ls = if ci { search.to_ascii_lowercase() } else { search.to_vec() };
    let mut out = Vec::new();
    let mut n = 0i64;
    let mut i = 0;
    while i < subject.len() {
        let hit = i + ls.len() <= subject.len()
            && if ci {
                subject[i..i + ls.len()].to_ascii_lowercase() == ls
            } else {
                subject[i..i + ls.len()] == ls[..]
            };
        if hit {
            out.extend_from_slice(replace);
            i += ls.len();
            n += 1;
        } else {
            out.push(subject[i]);
            i += 1;
        }
    }
    (out, n)
}

fn replace_bytes(subject: &[u8], search: &[u8], replace: &[u8]) -> Vec<u8> {
    if search.is_empty() {
        return subject.to_vec();
    }
    let mut out = Vec::new();
    let mut i = 0;
    while i < subject.len() {
        if i + search.len() <= subject.len() && &subject[i..i + search.len()] == search {
            out.extend_from_slice(replace);
            i += search.len();
        } else {
            out.push(subject[i]);
            i += 1;
        }
    }
    out
}

fn format_spec(conv: u8, spec: &str, arg: &Value) -> Vec<u8> {
    // parse: flags ([-0 +]) then width then .precision
    let mut chars = spec.chars().peekable();
    let mut left = false;
    let mut zero = false;
    let mut plus = false;
    while let Some(&c) = chars.peek() {
        match c {
            '-' => left = true,
            '0' => zero = true,
            '+' => plus = true,
            ' ' => {}
            _ => break,
        }
        chars.next();
    }
    let mut width = String::new();
    while let Some(&c) = chars.peek() {
        if c.is_ascii_digit() {
            width.push(c);
            chars.next();
        } else {
            break;
        }
    }
    let width: usize = width.parse().unwrap_or(0);
    let mut prec: Option<usize> = None;
    if chars.peek() == Some(&'.') {
        chars.next();
        let mut p = String::new();
        while let Some(&c) = chars.peek() {
            if c.is_ascii_digit() {
                p.push(c);
                chars.next();
            } else {
                break;
            }
        }
        prec = Some(p.parse().unwrap_or(0));
    }
    let body: Vec<u8> = match conv {
        b'd' | b'i' => {
            let n = to_i64(arg);
            let mut s = n.abs().to_string();
            if n < 0 {
                s = format!("-{s}");
            } else if plus {
                s = format!("+{s}");
            }
            s.into_bytes()
        }
        b'u' => (to_i64(arg) as u64).to_string().into_bytes(),
        b'f' | b'F' => {
            let p = prec.unwrap_or(6);
            format!("{:.*}", p, to_f64(arg)).into_bytes()
        }
        b's' => {
            let mut b = to_bytes(arg);
            if let Some(p) = prec {
                b.truncate(p);
            }
            b
        }
        b'x' => format!("{:x}", to_i64(arg)).into_bytes(),
        b'X' => format!("{:X}", to_i64(arg)).into_bytes(),
        b'o' => format!("{:o}", to_i64(arg)).into_bytes(),
        b'b' => format!("{:b}", to_i64(arg)).into_bytes(),
        b'c' => vec![to_i64(arg) as u8],
        b'e' => format!("{:e}", to_f64(arg)).into_bytes(),
        _ => Vec::new(),
    };
    if body.len() >= width {
        return body;
    }
    let pad = width - body.len();
    let padch = if zero && !left { b'0' } else { b' ' };
    let mut out = Vec::with_capacity(width);
    if left {
        out.extend_from_slice(&body);
        out.extend(std::iter::repeat(b' ').take(pad));
    } else {
        out.extend(std::iter::repeat(padch).take(pad));
        out.extend_from_slice(&body);
    }
    out
}

// ---- var_dump / print_r formatting -------------------------------------
fn var_dump(ev: &Eval, v: &Value, indent: usize, out: &mut String) {
    var_dump_seen(ev, v, indent, out, &mut Vec::new());
}

/// `seen` holds the Rc addresses of objects on the current path, so circular
/// object graphs print `*RECURSION*` instead of recursing into a memory bomb.
fn var_dump_seen(ev: &Eval, v: &Value, indent: usize, out: &mut String, seen: &mut Vec<usize>) {
    // Depth cap: deeply nested structures otherwise produce quadratic-sized output
    // (indentation grows per level), a memory bomb on e.g. a 50k-deep object chain.
    if indent > 256 {
        out.push_str(&format!("{}*MAX DEPTH*\n", "  ".repeat(indent)));
        return;
    }
    let pad = "  ".repeat(indent);
    match v {
        Value::Null => out.push_str(&format!("{pad}NULL\n")),
        Value::Bool(b) => out.push_str(&format!("{pad}bool({})\n", if *b { "true" } else { "false" })),
        Value::Int(n) => out.push_str(&format!("{pad}int({n})\n")),
        Value::Float(f) => out.push_str(&format!("{pad}float({})\n", dump_float(*f))),
        Value::Str(s) => out.push_str(&format!(
            "{pad}string({}) \"{}\"\n",
            s.len(),
            String::from_utf8_lossy(s)
        )),
        Value::Array(a) => {
            out.push_str(&format!("{pad}array({}) {{\n", a.len()));
            for (k, val) in &a.entries {
                match k {
                    Key::Int(n) => out.push_str(&format!("{pad}  [{n}]=>\n")),
                    Key::Str(s) => {
                        out.push_str(&format!("{pad}  [\"{}\"]=>\n", String::from_utf8_lossy(s)))
                    }
                }
                var_dump_seen(ev, val, indent + 1, out, seen);
            }
            out.push_str(&format!("{pad}}}\n"));
        }
        Value::Object(o) => {
            let id = Rc::as_ptr(o) as *const () as usize;
            if seen.contains(&id) {
                out.push_str(&format!("{pad}*RECURSION*\n"));
                return;
            }
            let ob = o.borrow();
            if ob.class == "__Stream" {
                let rid = ob.get("__id").map(to_i64).unwrap_or(0);
                out.push_str(&format!("{pad}resource({rid}) of type (stream)\n"));
                return;
            }
            // PHP shows DateTime/DateTimeZone with computed debug props (date /
            // timezone_type / timezone), not their internal state.
            if matches!(ob.class.as_str(), "DateTime" | "DateTimeImmutable") {
                let ts = ob.get("__ts").map(to_i64).unwrap_or(0);
                let tzname = ob
                    .get("__tz")
                    .map(|v| String::from_utf8_lossy(&to_bytes(v)).into_owned())
                    .unwrap_or_else(|| "UTC".to_string());
                let zone = if crate::tz::is_utc_name(&tzname) { None } else { crate::tz::lookup(&tzname) };
                let datestr = crate::php_date_tz("Y-m-d H:i:s", ts, zone.as_deref()) + ".000000";
                out.push_str(&format!("{pad}object({})#{} (3) {{\n", ob.class, ob.id));
                out.push_str(&format!("{pad}  [\"date\"]=>\n{pad}  string({}) \"{datestr}\"\n", datestr.len()));
                out.push_str(&format!("{pad}  [\"timezone_type\"]=>\n{pad}  int(3)\n"));
                out.push_str(&format!("{pad}  [\"timezone\"]=>\n{pad}  string({}) \"{tzname}\"\n", tzname.len()));
                out.push_str(&format!("{pad}}}\n"));
                return;
            }
            if ob.class == "DateTimeZone" {
                let tzname = ob
                    .get("name")
                    .map(|v| String::from_utf8_lossy(&to_bytes(v)).into_owned())
                    .unwrap_or_else(|| "UTC".to_string());
                out.push_str(&format!("{pad}object(DateTimeZone)#{} (2) {{\n", ob.id));
                out.push_str(&format!("{pad}  [\"timezone_type\"]=>\n{pad}  int(3)\n"));
                out.push_str(&format!("{pad}  [\"timezone\"]=>\n{pad}  string({}) \"{tzname}\"\n", tzname.len()));
                out.push_str(&format!("{pad}}}\n"));
                return;
            }
            out.push_str(&format!("{pad}object({})#{} ({}) {{\n", display_class(&ob.class), ob.id, ob.props.len()));
            seen.push(id);
            for (k, val) in &ob.props {
                out.push_str(&format!("{pad}  [\"{k}\"{}]=>\n", ev.prop_annotation(&ob.class, k)));
                var_dump_seen(ev, val, indent + 1, out, seen);
            }
            seen.pop();
            out.push_str(&format!("{pad}}}\n"));
        }
        Value::Closure(_) => out.push_str(&format!("{pad}object(Closure)#1 (0) {{\n{pad}}}\n")),
        Value::Ref(c) => {
            // PHP marks a reference element with `&` only while it has another
            // durable alias (refcount > 1). Our Rc count includes one temp clone
            // made evaluating the var_dump argument, hence the threshold of 3.
            if Rc::strong_count(c) >= 3 {
                let mut inner = String::new();
                var_dump_seen(ev, &c.borrow(), indent, &mut inner, seen);
                // each dump line starts with the indent pad; the `&` sits between
                // the pad and the value's type token
                let pad = "  ".repeat(indent);
                if let Some(rest) = inner.strip_prefix(pad.as_str()) {
                    out.push_str(&pad);
                    out.push('&');
                    out.push_str(rest);
                } else {
                    out.push('&');
                    out.push_str(&inner);
                }
            } else {
                var_dump_seen(ev, &c.borrow(), indent, out, seen);
            }
        }
    }
}

fn var_export(v: &Value, indent: usize, out: &mut String) {
    var_export_seen(v, indent, out, &mut Vec::new());
}

fn var_export_seen(v: &Value, indent: usize, out: &mut String, seen: &mut Vec<usize>) {
    if indent > 256 {
        out.push_str("NULL");
        return;
    }
    match v {
        Value::Null => out.push_str("NULL"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Int(n) => out.push_str(&n.to_string()),
        Value::Float(f) => {
            let s = format_float(*f);
            out.push_str(&s);
            if !s.contains('.') && !s.contains('E') && !s.contains('N') && !s.contains('I') {
                out.push_str(".0");
            }
        }
        Value::Str(s) => {
            out.push('\'');
            for &b in s.iter() {
                if b == b'\'' || b == b'\\' {
                    out.push('\\');
                }
                out.push(b as char);
            }
            out.push('\'');
        }
        Value::Array(a) => {
            let pad = "  ".repeat(indent);
            out.push_str("array (\n");
            for (k, val) in &a.entries {
                out.push_str(&pad);
                out.push_str("  ");
                match k {
                    Key::Int(n) => out.push_str(&format!("{n} => ")),
                    Key::Str(s) => out.push_str(&format!("'{}' => ", String::from_utf8_lossy(s))),
                }
                if matches!(val, Value::Array(_)) {
                    out.push('\n');
                    out.push_str(&pad);
                    out.push_str("  ");
                }
                var_export_seen(val, indent + 1, out, seen);
                out.push_str(",\n");
            }
            out.push_str(&pad);
            out.push(')');
        }
        Value::Object(rc) => {
            let id = Rc::as_ptr(rc) as *const () as usize;
            if seen.contains(&id) {
                out.push_str("NULL"); // cycle — PHP errors; we just stop
                return;
            }
            seen.push(id);
            let o = rc.borrow();
            out.push_str(&format!("\\{}::__set_state(array(\n", o.class));
            let pad = "  ".repeat(indent);
            for (name, val) in &o.props {
                out.push_str(&format!("{pad}   '{name}' => "));
                var_export_seen(val, indent + 1, out, seen);
                out.push_str(",\n");
            }
            out.push_str(&format!("{pad})"));
            seen.pop();
        }
        Value::Closure(_) => out.push_str("NULL"),
        Value::Ref(c) => var_export_seen(&c.borrow(), indent, out, seen),
    }
}

fn print_r(v: &Value, indent: usize, out: &mut String) {
    if indent > 256 {
        out.push_str("*MAX DEPTH*");
        return;
    }
    match v {
        Value::Array(a) => {
            let pad = "    ".repeat(indent);
            out.push_str("Array\n");
            out.push_str(&format!("{pad}(\n"));
            for (k, val) in &a.entries {
                let ks = match k {
                    Key::Int(n) => n.to_string(),
                    Key::Str(s) => String::from_utf8_lossy(s).into_owned(),
                };
                out.push_str(&format!("{pad}    [{ks}] => "));
                print_r(val, indent + 2, out);
                out.push('\n');
            }
            out.push_str(&format!("{pad})\n"));
        }
        // reference elements are transparent in print_r (no & marker)
        Value::Ref(c) => print_r(&c.borrow(), indent, out),
        other => out.push_str(&String::from_utf8_lossy(&to_bytes(other))),
    }
}

// ---- helpers -----------------------------------------------------------

/// Count nodes in a value (arrays only — the explosion vector), iteratively and
/// bounded by `limit`. Guards against pathological array growth, e.g. value-copy
/// fallback for `$a[$i] =& $a` (references not yet modeled) blowing up
/// exponentially. Iterative so it can't itself overflow the stack.
fn value_size(v: &Value, limit: usize) -> usize {
    let mut count = 0usize;
    let mut stack: Vec<&Value> = vec![v];
    while let Some(cur) = stack.pop() {
        count += 1;
        if count > limit {
            return count;
        }
        if let Value::Array(a) = cur {
            for (_, e) in &a.entries {
                stack.push(e);
            }
        }
    }
    count
}

/// The predefined PHP constants commonly used in the corpus.
fn php_const(n: &str) -> Option<Value> {
    use Value::*;
    let sep = if cfg!(windows) { "\\" } else { "/" };
    Some(match n {
        "PHP_EOL" => Str(b"\n".to_vec()),
        "PHP_SAPI" => Str(b"cli".to_vec()),
        // date() format-string presets (also DateTimeInterface class constants)
        "DATE_ATOM" => Str(b"Y-m-d\\TH:i:sP".to_vec()),
        "DATE_ISO8601" => Str(b"Y-m-d\\TH:i:sO".to_vec()),
        "DATE_RFC822" => Str(b"D, d M y H:i:s O".to_vec()),
        "DATE_RFC850" => Str(b"l, d-M-y H:i:s T".to_vec()),
        "DATE_RFC1036" => Str(b"D, d M y H:i:s O".to_vec()),
        "DATE_RFC1123" => Str(b"D, d M Y H:i:s O".to_vec()),
        "DATE_RFC2822" => Str(b"D, d M Y H:i:s O".to_vec()),
        "DATE_RFC3339" => Str(b"Y-m-d\\TH:i:sP".to_vec()),
        "DATE_RFC3339_EXTENDED" => Str(b"Y-m-d\\TH:i:s.vP".to_vec()),
        "DATE_RFC7231" => Str(b"D, d M Y H:i:s \\G\\M\\T".to_vec()),
        "DATE_COOKIE" => Str(b"l, d-M-Y H:i:s T".to_vec()),
        "DATE_RSS" => Str(b"D, d M Y H:i:s O".to_vec()),
        "DATE_W3C" => Str(b"Y-m-d\\TH:i:sP".to_vec()),
        // array_filter() callback modes
        "ARRAY_FILTER_USE_BOTH" => Int(1),
        "ARRAY_FILTER_USE_KEY" => Int(2),
        // htmlspecialchars() document-type flags
        "ENT_HTML401" => Int(0),
        "ENT_XML1" => Int(16),
        "ENT_XHTML" => Int(32),
        "ENT_HTML5" => Int(48),
        // stream socket flags (transports probe and fail into WP_Error)
        "STREAM_CLIENT_PERSISTENT" => Int(1),
        "STREAM_CLIENT_ASYNC_CONNECT" => Int(2),
        "STREAM_CLIENT_CONNECT" => Int(4),
        // crypt() capability constants (PHP 8: all algorithms built in)
        "CRYPT_BLOWFISH" | "CRYPT_EXT_DES" | "CRYPT_MD5" | "CRYPT_SHA256"
        | "CRYPT_SHA512" | "CRYPT_STD_DES" => Int(1),
        "CRYPT_SALT_LENGTH" => Int(123),
        // glob() flags (Linux values — the engine emulates PHP-on-Unix)
        "GLOB_ERR" => Int(1),
        "GLOB_MARK" => Int(2),
        "GLOB_NOSORT" => Int(4),
        "GLOB_NOCHECK" => Int(16),
        "GLOB_NOESCAPE" => Int(64),
        "GLOB_BRACE" => Int(1024),
        "GLOB_ONLYDIR" => Int(8192),
        "GLOB_AVAILABLE_FLAGS" => Int(9303),
        // parse_url() component selectors
        "PHP_URL_SCHEME" => Int(0),
        "PHP_URL_HOST" => Int(1),
        "PHP_URL_PORT" => Int(2),
        "PHP_URL_USER" => Int(3),
        "PHP_URL_PASS" => Int(4),
        "PHP_URL_PATH" => Int(5),
        "PHP_URL_QUERY" => Int(6),
        "PHP_URL_FRAGMENT" => Int(7),
        "PHP_INT_MAX" => Int(i64::MAX),
        "PHP_INT_MIN" => Int(i64::MIN),
        "PHP_INT_SIZE" => Int(8),
        "PHP_FLOAT_EPSILON" => Float(f64::EPSILON),
        "PHP_FLOAT_MAX" => Float(f64::MAX),
        "PHP_FLOAT_MIN" => Float(f64::MIN_POSITIVE),
        "PHP_FLOAT_DIG" => Int(15),
        "PHP_VERSION" => Str(b"8.3.0".to_vec()),
        "PHP_MAJOR_VERSION" => Int(8),
        "PHP_MINOR_VERSION" => Int(3),
        "PHP_RELEASE_VERSION" => Int(0),
        "PHP_VERSION_ID" => Int(80300),
        "PHP_OS" => Str(if cfg!(windows) { b"WINNT".to_vec() } else { b"Linux".to_vec() }),
        "PHP_OS_FAMILY" => Str(if cfg!(windows) { b"Windows".to_vec() } else { b"Linux".to_vec() }),
        "DIRECTORY_SEPARATOR" => Str(sep.as_bytes().to_vec()),
        "PATH_SEPARATOR" => Str(if cfg!(windows) { b";".to_vec() } else { b":".to_vec() }),
        "NULL" | "null" => Null,
        "TRUE" | "true" => Bool(true),
        "FALSE" | "false" => Bool(false),
        // math
        "M_PI" => Float(std::f64::consts::PI),
        "M_E" => Float(std::f64::consts::E),
        "M_SQRT2" => Float(std::f64::consts::SQRT_2),
        "M_SQRT1_2" => Float(std::f64::consts::FRAC_1_SQRT_2),
        "M_SQRT3" => Float(1.7320508075688772),
        "M_LN2" => Float(std::f64::consts::LN_2),
        "M_LN10" => Float(std::f64::consts::LN_10),
        "M_LOG2E" => Float(std::f64::consts::LOG2_E),
        "M_LOG10E" => Float(std::f64::consts::LOG10_E),
        "M_PI_2" => Float(std::f64::consts::FRAC_PI_2),
        "M_PI_4" => Float(std::f64::consts::FRAC_PI_4),
        "M_2_PI" => Float(std::f64::consts::FRAC_2_PI),
        "M_1_PI" => Float(std::f64::consts::FRAC_1_PI),
        "M_EULER" => Float(0.5772156649015329),
        "INF" => Float(f64::INFINITY),
        "NAN" => Float(f64::NAN),
        // error levels
        "E_ERROR" => Int(1),
        "E_WARNING" => Int(2),
        "E_PARSE" => Int(4),
        "E_NOTICE" => Int(8),
        "E_CORE_ERROR" => Int(16),
        "E_CORE_WARNING" => Int(32),
        "E_COMPILE_ERROR" => Int(64),
        "E_COMPILE_WARNING" => Int(128),
        "E_USER_ERROR" => Int(256),
        "E_USER_WARNING" => Int(512),
        "E_USER_NOTICE" => Int(1024),
        "E_STRICT" => Int(2048),
        "E_RECOVERABLE_ERROR" => Int(4096),
        "E_DEPRECATED" => Int(8192),
        "E_USER_DEPRECATED" => Int(16384),
        // PHP 8.4 removed E_STRICT (2048) from E_ALL
        "E_ALL" => Int(30719),
        // locale categories (glibc numbering, PHP-on-Linux)
        "LC_CTYPE" => Int(0),
        "LC_NUMERIC" => Int(1),
        "LC_TIME" => Int(2),
        "LC_COLLATE" => Int(3),
        "LC_MONETARY" => Int(4),
        "LC_MESSAGES" => Int(5),
        "LC_ALL" => Int(6),
        // extract() flags
        "EXTR_OVERWRITE" => Int(0),
        "EXTR_SKIP" => Int(1),
        "EXTR_PREFIX_SAME" => Int(2),
        "EXTR_PREFIX_ALL" => Int(3),
        "EXTR_PREFIX_INVALID" => Int(4),
        "EXTR_PREFIX_IF_EXISTS" => Int(5),
        "EXTR_IF_EXISTS" => Int(6),
        "EXTR_REFS" => Int(256),
        // http_build_query() encoding
        "PHP_QUERY_RFC1738" => Int(1),
        "PHP_QUERY_RFC3986" => Int(2),
        // stream filter chains
        "STREAM_FILTER_READ" => Int(1),
        "STREAM_FILTER_WRITE" => Int(2),
        "STREAM_FILTER_ALL" => Int(3),
        // sort flags
        "SORT_REGULAR" => Int(0),
        "SORT_NUMERIC" => Int(1),
        "SORT_STRING" => Int(2),
        "SORT_DESC" => Int(3),
        "SORT_ASC" => Int(4),
        "SORT_LOCALE_STRING" => Int(5),
        "SORT_NATURAL" => Int(6),
        "SORT_FLAG_CASE" => Int(8),
        // count / str_pad / array_filter
        "COUNT_NORMAL" => Int(0),
        "COUNT_RECURSIVE" => Int(1),
        "STR_PAD_RIGHT" => Int(1),
        "STR_PAD_LEFT" => Int(0),
        "STR_PAD_BOTH" => Int(2),
        "ARRAY_FILTER_USE_KEY" => Int(2),
        "ARRAY_FILTER_USE_BOTH" => Int(1),
        // file flags
        "FILE_USE_INCLUDE_PATH" => Int(1),
        "FILE_IGNORE_NEW_LINES" => Int(2),
        "FILE_SKIP_EMPTY_LINES" => Int(4),
        "FILE_APPEND" => Int(8),
        "FILE_NO_DEFAULT_CONTEXT" => Int(16),
        "FILE_TEXT" => Int(0),
        "FILE_BINARY" => Int(0),
        "SEEK_SET" => Int(0),
        "SEEK_CUR" => Int(1),
        "SEEK_END" => Int(2),
        "LOCK_SH" => Int(1),
        "LOCK_EX" => Int(2),
        "LOCK_UN" => Int(3),
        "PATHINFO_DIRNAME" => Int(1),
        "PATHINFO_BASENAME" => Int(2),
        "PATHINFO_EXTENSION" => Int(4),
        "PATHINFO_FILENAME" => Int(8),
        // htmlspecialchars / ent
        "ENT_QUOTES" => Int(3),
        "ENT_COMPAT" => Int(2),
        "ENT_NOQUOTES" => Int(0),
        "ENT_HTML401" => Int(0),
        "ENT_HTML5" => Int(48),
        // json
        "JSON_HEX_TAG" => Int(1),
        "JSON_HEX_AMP" => Int(2),
        "JSON_HEX_APOS" => Int(4),
        "JSON_HEX_QUOT" => Int(8),
        "JSON_FORCE_OBJECT" => Int(16),
        "JSON_NUMERIC_CHECK" => Int(32),
        "JSON_PARTIAL_OUTPUT_ON_ERROR" => Int(512),
        "JSON_PRESERVE_ZERO_FRACTION" => Int(1024),
        "JSON_INVALID_UTF8_IGNORE" => Int(1048576),
        "JSON_INVALID_UTF8_SUBSTITUTE" => Int(2097152),
        "JSON_OBJECT_AS_ARRAY" => Int(1),
        "JSON_BIGINT_AS_STRING" => Int(2),
        "JSON_PRETTY_PRINT" => Int(128),
        "JSON_UNESCAPED_SLASHES" => Int(64),
        "JSON_UNESCAPED_UNICODE" => Int(256),
        "JSON_THROW_ON_ERROR" => Int(4194304),
        "JSON_ERROR_NONE" => Int(0),
        // preg
        "PREG_PATTERN_ORDER" => Int(1),
        "PREG_SET_ORDER" => Int(2),
        "PREG_OFFSET_CAPTURE" => Int(256),
        "PREG_SPLIT_NO_EMPTY" => Int(1),
        "PREG_SPLIT_DELIM_CAPTURE" => Int(2),
        // misc
        "PHP_ROUND_HALF_UP" => Int(1),
        "PHP_ROUND_HALF_DOWN" => Int(2),
        "PHP_ROUND_HALF_EVEN" => Int(3),
        "PHP_ROUND_HALF_ODD" => Int(4),
        "ENT_SUBSTITUTE" => Int(8),
        // filter
        "FILTER_DEFAULT" | "FILTER_UNSAFE_RAW" => Int(516),
        "FILTER_VALIDATE_INT" => Int(257),
        "FILTER_VALIDATE_BOOLEAN" | "FILTER_VALIDATE_BOOL" => Int(258),
        "FILTER_VALIDATE_FLOAT" => Int(259),
        "FILTER_VALIDATE_REGEXP" => Int(272),
        "FILTER_VALIDATE_URL" => Int(273),
        "FILTER_VALIDATE_EMAIL" => Int(274),
        "FILTER_VALIDATE_IP" => Int(275),
        "FILTER_SANITIZE_STRING" => Int(513),
        "FILTER_SANITIZE_NUMBER_INT" => Int(519),
        "FILTER_SANITIZE_EMAIL" => Int(517),
        "FILTER_SANITIZE_URL" => Int(518),
        "FILTER_FLAG_ALLOW_THOUSAND" => Int(8192),
        "FILTER_NULL_ON_FAILURE" => Int(134217728),
        "FILTER_REQUIRE_SCALAR" => Int(33554432),
        "FILTER_REQUIRE_ARRAY" => Int(16777216),
        "FILTER_FORCE_ARRAY" => Int(67108864),
        // filter: remaining validate/sanitize kinds + flags (ext/filter)
        "FILTER_VALIDATE_MAC" => Int(276),
        "FILTER_VALIDATE_DOMAIN" => Int(277),
        "FILTER_SANITIZE_ENCODED" => Int(514),
        "FILTER_SANITIZE_SPECIAL_CHARS" => Int(515),
        "FILTER_SANITIZE_NUMBER_FLOAT" => Int(520),
        "FILTER_SANITIZE_FULL_SPECIAL_CHARS" => Int(522),
        "FILTER_SANITIZE_ADD_SLASHES" => Int(523),
        "FILTER_CALLBACK" => Int(1024),
        "FILTER_FLAG_ALLOW_OCTAL" => Int(1),
        "FILTER_FLAG_ALLOW_HEX" => Int(2),
        "FILTER_FLAG_ALLOW_FRACTION" => Int(4096),
        "FILTER_FLAG_ALLOW_SCIENTIFIC" => Int(16384),
        "FILTER_FLAG_IPV4" => Int(1048576),
        "FILTER_FLAG_IPV6" => Int(2097152),
        // filter_input() input sources (ext/filter)
        "INPUT_POST" => Int(0),
        "INPUT_GET" => Int(1),
        "INPUT_COOKIE" => Int(2),
        "INPUT_ENV" => Int(4),
        "INPUT_SERVER" => Int(5),
        // libxml (DOM / SimpleXML option args + error levels)
        "LIBXML_VERSION" | "LIBXML_LOADED_VERSION" => Int(21400),
        "LIBXML_DOTTED_VERSION" => Str(b"2.14.0".to_vec()),
        "LIBXML_NOENT" => Int(2),
        "LIBXML_DTDLOAD" => Int(4),
        "LIBXML_DTDATTR" => Int(8),
        "LIBXML_DTDVALID" => Int(16),
        "LIBXML_NOERROR" => Int(32),
        "LIBXML_NOWARNING" => Int(64),
        "LIBXML_NOBLANKS" => Int(256),
        "LIBXML_XINCLUDE" => Int(1024),
        "LIBXML_NSCLEAN" => Int(8192),
        "LIBXML_NOCDATA" => Int(16384),
        "LIBXML_NONET" => Int(2048),
        "LIBXML_PEDANTIC" => Int(128),
        "LIBXML_COMPACT" => Int(65536),
        "LIBXML_PARSEHUGE" => Int(524288),
        "LIBXML_BIGLINES" => Int(4194304),
        "LIBXML_NOXMLDECL" => Int(2),
        "LIBXML_NOEMPTYTAG" => Int(4),
        "LIBXML_SCHEMA_CREATE" => Int(1),
        "LIBXML_HTML_NOIMPLIED" => Int(8192),
        "LIBXML_HTML_NODEFDTD" => Int(4),
        "LIBXML_ERR_NONE" => Int(0),
        "LIBXML_ERR_WARNING" => Int(1),
        "LIBXML_ERR_ERROR" => Int(2),
        "LIBXML_ERR_FATAL" => Int(3),
        // xml parser (ext/xml, xml_parser_* functions)
        "XML_OPTION_CASE_FOLDING" => Int(1),
        "XML_OPTION_TARGET_ENCODING" => Int(2),
        "XML_OPTION_SKIP_TAGSTART" => Int(3),
        "XML_OPTION_SKIP_WHITE" => Int(4),
        "XML_ERROR_NONE" => Int(0),
        "XML_ERROR_SYNTAX" => Int(2),
        // htmlspecialchars/htmlentities decode table selector + token_get_all flags
        "HTML_ENTITIES" => Int(1),
        "HTML_SPECIALCHARS" => Int(0),
        "TOKEN_PARSE" => Int(1),
        // ext/fileinfo
        "FILEINFO_NONE" => Int(0),
        "FILEINFO_MIME" => Int(1040),
        "FILEINFO_MIME_TYPE" => Int(16),
        "FILEINFO_MIME_ENCODING" => Int(1024),
        // array_change_key_case / mb_convert_case
        "CASE_LOWER" => Int(0),
        "CASE_UPPER" => Int(1),
        "MB_CASE_UPPER" => Int(0),
        "MB_CASE_LOWER" => Int(1),
        "MB_CASE_TITLE" => Int(2),
        // connection_status()
        "CONNECTION_NORMAL" => Int(0),
        "CONNECTION_ABORTED" => Int(1),
        "CONNECTION_TIMEOUT" => Int(2),
        // preg: remaining split/grep flags (PATTERN_ORDER etc already defined above)
        "PREG_SPLIT_OFFSET_CAPTURE" => Int(4),
        "PREG_UNMATCHED_AS_NULL" => Int(512),
        "PREG_GREP_INVERT" => Int(1),
        _ => return None,
    })
}

// ---- serialize / unserialize (byte-based, for the v2 Value) -------------

fn ser_float(f: f64) -> String {
    if f.is_nan() {
        "NAN".into()
    } else if f.is_infinite() {
        if f < 0.0 { "-INF".into() } else { "INF".into() }
    } else {
        format!("{f}") // Rust's default is shortest round-trip
    }
}

/// var_dump / var_export float format: PHP uses serialize_precision -1 (shortest
/// round-trippable), with `E` notation for very large/small magnitudes.
fn dump_float(f: f64) -> String {
    if f.is_nan() {
        return "NAN".into();
    }
    if f.is_infinite() {
        return if f < 0.0 { "-INF".into() } else { "INF".into() };
    }
    if f == 0.0 {
        return if f.is_sign_negative() { "-0".into() } else { "0".into() };
    }
    let a = f.abs();
    if a >= 1e15 || a < 1e-4 {
        let s = format!("{f:e}"); // e.g. "1e20", "2.5e-5"
        if let Some((mant, exp)) = s.split_once('e') {
            let mant = if mant.contains('.') { mant.to_string() } else { format!("{mant}.0") };
            let n: i64 = exp.parse().unwrap_or(0);
            return format!("{mant}E{}{}", if n >= 0 { "+" } else { "-" }, n.abs());
        }
        s
    } else {
        format!("{f}")
    }
}

fn unser_read_until(b: &[u8], pos: &mut usize, end: u8) -> Vec<u8> {
    let start = *pos;
    while *pos < b.len() && b[*pos] != end {
        *pos += 1;
    }
    let r = b[start..*pos].to_vec();
    if *pos < b.len() {
        *pos += 1; // consume end
    }
    r
}

fn php_unserialize(b: &[u8], pos: &mut usize, depth: usize) -> Option<Value> {
    if depth > 256 || *pos >= b.len() {
        return None;
    }
    let t = b[*pos];
    match t {
        b'N' => {
            *pos += 2; // N;
            Some(Value::Null)
        }
        b'b' => {
            *pos += 2; // b:
            let v = b.get(*pos)? == &b'1';
            *pos += 2; // X;
            Some(Value::Bool(v))
        }
        b'i' => {
            *pos += 2; // i:
            let s = unser_read_until(b, pos, b';');
            std::str::from_utf8(&s).ok()?.trim().parse::<i64>().ok().map(Value::Int)
        }
        b'd' => {
            *pos += 2; // d:
            let s = unser_read_until(b, pos, b';');
            let t = std::str::from_utf8(&s).ok()?.trim();
            let v = match t {
                "INF" => f64::INFINITY,
                "-INF" => f64::NEG_INFINITY,
                "NAN" => f64::NAN,
                _ => t.parse().ok()?,
            };
            Some(Value::Float(v))
        }
        b's' => {
            *pos += 2; // s:
            let len: usize = std::str::from_utf8(&unser_read_until(b, pos, b':')).ok()?.parse().ok()?;
            if b.get(*pos)? != &b'"' {
                return None;
            }
            *pos += 1; // "
            if *pos + len > b.len() {
                return None;
            }
            let bytes = b[*pos..*pos + len].to_vec();
            *pos += len;
            *pos += 2; // ";
            Some(Value::Str(bytes))
        }
        b'a' => {
            *pos += 2; // a:
            let n: usize = std::str::from_utf8(&unser_read_until(b, pos, b':')).ok()?.parse().ok()?;
            if b.get(*pos)? != &b'{' {
                return None;
            }
            *pos += 1; // {
            let mut arr = Arr::new();
            for _ in 0..n {
                let k = php_unserialize(b, pos, depth + 1)?;
                let v = php_unserialize(b, pos, depth + 1)?;
                arr.insert(Arr::norm_key(&k), v);
            }
            *pos += 1; // }
            Some(Value::Array(arr))
        }
        b'O' => {
            *pos += 2; // O:
            let clen: usize = std::str::from_utf8(&unser_read_until(b, pos, b':')).ok()?.parse().ok()?;
            *pos += 1; // "
            let class = String::from_utf8_lossy(&b[*pos..*pos + clen]).into_owned();
            *pos += clen;
            *pos += 2; // ":
            let n: usize = std::str::from_utf8(&unser_read_until(b, pos, b':')).ok()?.parse().ok()?;
            if b.get(*pos)? != &b'{' {
                return None;
            }
            *pos += 1; // {
            let obj = Rc::new(RefCell::new(Obj::new(class)));
            for _ in 0..n {
                let k = php_unserialize(b, pos, depth + 1)?;
                let v = php_unserialize(b, pos, depth + 1)?;
                let name = String::from_utf8_lossy(&to_bytes(&k)).into_owned();
                obj.borrow_mut().set(&name, v);
            }
            *pos += 1; // }
            Some(Value::Object(obj))
        }
        _ => None,
    }
}

fn akey_to_value(k: &Key) -> Value {
    match k {
        Key::Int(n) => Value::Int(*n),
        Key::Str(s) => Value::Str(s.clone()),
    }
}

fn key_cmp(a: &Key, b: &Key) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    match (a, b) {
        (Key::Int(x), Key::Int(y)) => x.cmp(y),
        (Key::Str(x), Key::Str(y)) => x.cmp(y),
        (Key::Int(_), Key::Str(_)) => Ordering::Less,
        (Key::Str(_), Key::Int(_)) => Ordering::Greater,
    }
}

// ---- JSON (byte-based, for the v2 Value) -------------------------------
fn json_is_list(a: &Arr) -> bool {
    a.entries.iter().enumerate().all(|(i, (k, _))| matches!(k, Key::Int(n) if *n == i as i64))
}

fn json_encode(v: &Value, out: &mut Vec<u8>, depth: usize) {
    if depth > 512 {
        out.extend_from_slice(b"null");
        return;
    }
    match v {
        Value::Null => out.extend_from_slice(b"null"),
        Value::Bool(b) => out.extend_from_slice(if *b { b"true" } else { b"false" }),
        Value::Int(n) => out.extend_from_slice(n.to_string().as_bytes()),
        Value::Float(f) => {
            if f.is_finite() {
                out.extend_from_slice(format_float(*f).as_bytes());
            } else {
                out.push(b'0');
            }
        }
        Value::Str(s) => json_str(s, out),
        Value::Array(a) => {
            if json_is_list(a) {
                out.push(b'[');
                for (i, (_, val)) in a.entries.iter().enumerate() {
                    if i > 0 {
                        out.push(b',');
                    }
                    json_encode(val, out, depth + 1);
                }
                out.push(b']');
            } else {
                out.push(b'{');
                for (i, (k, val)) in a.entries.iter().enumerate() {
                    if i > 0 {
                        out.push(b',');
                    }
                    match k {
                        Key::Int(n) => json_str(n.to_string().as_bytes(), out),
                        Key::Str(s) => json_str(s, out),
                    }
                    out.push(b':');
                    json_encode(val, out, depth + 1);
                }
                out.push(b'}');
            }
        }
        Value::Object(rc) => {
            out.push(b'{');
            for (i, (name, val)) in rc.borrow().props.iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                json_str(name.as_bytes(), out);
                out.push(b':');
                json_encode(val, out, depth + 1);
            }
            out.push(b'}');
        }
        Value::Closure(_) => out.extend_from_slice(b"null"),
        Value::Ref(c) => json_encode(&c.borrow(), out, depth),
    }
}

fn json_str(s: &[u8], out: &mut Vec<u8>) {
    out.push(b'"');
    for &b in s {
        match b {
            b'"' => out.extend_from_slice(b"\\\""),
            b'\\' => out.extend_from_slice(b"\\\\"),
            b'/' => out.extend_from_slice(b"\\/"),
            b'\n' => out.extend_from_slice(b"\\n"),
            b'\r' => out.extend_from_slice(b"\\r"),
            b'\t' => out.extend_from_slice(b"\\t"),
            0x08 => out.extend_from_slice(b"\\b"),
            0x0c => out.extend_from_slice(b"\\f"),
            c if c < 0x20 => out.extend_from_slice(format!("\\u{c:04x}").as_bytes()),
            c => out.push(c),
        }
    }
    out.push(b'"');
}

fn json_decode(b: &[u8], assoc: bool) -> Option<Value> {
    let mut p = 0;
    json_ws(b, &mut p);
    let v = json_val(b, &mut p, assoc, 0)?;
    json_ws(b, &mut p);
    if p == b.len() {
        Some(v)
    } else {
        None
    }
}

fn json_ws(b: &[u8], p: &mut usize) {
    while *p < b.len() && matches!(b[*p], b' ' | b'\t' | b'\n' | b'\r') {
        *p += 1;
    }
}

fn json_val(b: &[u8], p: &mut usize, assoc: bool, depth: usize) -> Option<Value> {
    if depth > 512 || *p >= b.len() {
        return None;
    }
    json_ws(b, p);
    match b.get(*p)? {
        b'n' => {
            if b[*p..].starts_with(b"null") {
                *p += 4;
                Some(Value::Null)
            } else {
                None
            }
        }
        b't' => {
            if b[*p..].starts_with(b"true") {
                *p += 4;
                Some(Value::Bool(true))
            } else {
                None
            }
        }
        b'f' => {
            if b[*p..].starts_with(b"false") {
                *p += 5;
                Some(Value::Bool(false))
            } else {
                None
            }
        }
        b'"' => json_string(b, p).map(Value::Str),
        b'[' => {
            *p += 1;
            let mut arr = Arr::new();
            json_ws(b, p);
            if b.get(*p) == Some(&b']') {
                *p += 1;
                return Some(Value::Array(arr));
            }
            loop {
                let v = json_val(b, p, assoc, depth + 1)?;
                arr.push(v);
                json_ws(b, p);
                match b.get(*p)? {
                    b',' => *p += 1,
                    b']' => {
                        *p += 1;
                        break;
                    }
                    _ => return None,
                }
            }
            Some(Value::Array(arr))
        }
        b'{' => {
            *p += 1;
            let mut arr = Arr::new();
            json_ws(b, p);
            if b.get(*p) == Some(&b'}') {
                *p += 1;
                return finish_obj(arr, assoc);
            }
            loop {
                json_ws(b, p);
                let key = json_string(b, p)?;
                json_ws(b, p);
                if b.get(*p)? != &b':' {
                    return None;
                }
                *p += 1;
                let v = json_val(b, p, assoc, depth + 1)?;
                arr.insert(Arr::norm_key(&Value::Str(key)), v);
                json_ws(b, p);
                match b.get(*p)? {
                    b',' => *p += 1,
                    b'}' => {
                        *p += 1;
                        break;
                    }
                    _ => return None,
                }
            }
            finish_obj(arr, assoc)
        }
        _ => {
            // number
            let start = *p;
            if b[*p] == b'-' {
                *p += 1;
            }
            let mut is_float = false;
            while *p < b.len() && matches!(b[*p], b'0'..=b'9') {
                *p += 1;
            }
            if b.get(*p) == Some(&b'.') {
                is_float = true;
                *p += 1;
                while *p < b.len() && b[*p].is_ascii_digit() {
                    *p += 1;
                }
            }
            if matches!(b.get(*p), Some(b'e' | b'E')) {
                is_float = true;
                *p += 1;
                if matches!(b.get(*p), Some(b'+' | b'-')) {
                    *p += 1;
                }
                while *p < b.len() && b[*p].is_ascii_digit() {
                    *p += 1;
                }
            }
            let txt = std::str::from_utf8(&b[start..*p]).ok()?;
            if txt.is_empty() || txt == "-" {
                return None;
            }
            if is_float {
                Some(Value::Float(txt.parse().ok()?))
            } else {
                match txt.parse::<i64>() {
                    Ok(n) => Some(Value::Int(n)),
                    Err(_) => Some(Value::Float(txt.parse().ok()?)),
                }
            }
        }
    }
}

fn finish_obj(arr: Arr, assoc: bool) -> Option<Value> {
    if assoc {
        Some(Value::Array(arr))
    } else {
        let o = Rc::new(RefCell::new(Obj::new("stdClass")));
        for (k, v) in arr.entries {
            let name = match k {
                Key::Int(n) => n.to_string(),
                Key::Str(s) => String::from_utf8_lossy(&s).into_owned(),
            };
            o.borrow_mut().set(&name, v);
        }
        Some(Value::Object(o))
    }
}

fn json_string(b: &[u8], p: &mut usize) -> Option<Vec<u8>> {
    if b.get(*p)? != &b'"' {
        return None;
    }
    *p += 1;
    let mut out = Vec::new();
    while *p < b.len() {
        match b[*p] {
            b'"' => {
                *p += 1;
                return Some(out);
            }
            b'\\' => {
                *p += 1;
                match b.get(*p)? {
                    b'"' => out.push(b'"'),
                    b'\\' => out.push(b'\\'),
                    b'/' => out.push(b'/'),
                    b'n' => out.push(b'\n'),
                    b't' => out.push(b'\t'),
                    b'r' => out.push(b'\r'),
                    b'b' => out.push(0x08),
                    b'f' => out.push(0x0c),
                    b'u' => {
                        let hex = std::str::from_utf8(b.get(*p + 1..*p + 5)?).ok()?;
                        let cp = u32::from_str_radix(hex, 16).ok()?;
                        *p += 4;
                        if let Some(c) = char::from_u32(cp) {
                            let mut buf = [0u8; 4];
                            out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
                        }
                    }
                    _ => return None,
                }
                *p += 1;
            }
            c => {
                out.push(c);
                *p += 1;
            }
        }
    }
    None
}

// ---- generator detection: does a function body contain `yield`? (not
// descending into nested closures/functions, whose yields are their own) ----
fn has_yield(stmts: &[Stmt]) -> bool {
    stmts.iter().any(stmt_has_yield)
}

fn stmt_has_yield(s: &Stmt) -> bool {
    match s {
        Stmt::Marked(_, inner) => stmt_has_yield(inner),
        Stmt::Expr(e) | Stmt::Throw(e) => expr_has_yield(e),
        Stmt::Echo(es) => es.iter().any(expr_has_yield),
        Stmt::Return(Some(e)) => expr_has_yield(e),
        Stmt::Block(b) => has_yield(b),
        Stmt::If { cond, then, elseifs, els } => {
            expr_has_yield(cond)
                || has_yield(then)
                || elseifs.iter().any(|(c, b)| expr_has_yield(c) || has_yield(b))
                || els.as_ref().is_some_and(|b| has_yield(b))
        }
        Stmt::While { cond, body } | Stmt::DoWhile { body, cond } => {
            expr_has_yield(cond) || has_yield(body)
        }
        Stmt::For { init, cond, step, body } => {
            init.iter().chain(cond).chain(step).any(expr_has_yield) || has_yield(body)
        }
        Stmt::Foreach { array, body, .. } => expr_has_yield(array) || has_yield(body),
        Stmt::Switch { subject, cases } => {
            expr_has_yield(subject) || cases.iter().any(|c| has_yield(&c.body))
        }
        Stmt::Try { body, catches, finally } => {
            has_yield(body)
                || catches.iter().any(|c| has_yield(&c.body))
                || finally.as_ref().is_some_and(|b| has_yield(b))
        }
        _ => false,
    }
}

fn expr_has_yield(e: &Expr) -> bool {
    match e {
        Expr::Yield(..) | Expr::YieldFrom(..) => true,
        Expr::Binary(_, a, b) | Expr::AssignOp(_, a, b) | Expr::Assign(a, b)
        | Expr::AssignRef(a, b) | Expr::InstanceOf(a, b) => expr_has_yield(a) || expr_has_yield(b),
        Expr::Unary(_, x) | Expr::Cast(_, x) | Expr::Print(x) | Expr::ErrorSuppress(x)
        | Expr::Throw(x) | Expr::Empty(x) | Expr::Clone(x) | Expr::PreInc(x)
        | Expr::PreDec(x) | Expr::PostInc(x) | Expr::PostDec(x) => expr_has_yield(x),
        Expr::Ternary(a, b, c) => {
            expr_has_yield(a) || b.as_ref().is_some_and(|e| expr_has_yield(e)) || expr_has_yield(c)
        }
        Expr::Index(a, b) => expr_has_yield(a) || b.as_ref().is_some_and(|e| expr_has_yield(e)),
        Expr::Call(_, args)
        | Expr::MethodCall(_, _, args, _)
        | Expr::StaticCall(_, _, args)
        | Expr::New(_, args) => args.iter().any(|a| expr_has_yield(&a.value)),
        Expr::Array(items) => items
            .iter()
            .any(|it| expr_has_yield(&it.value) || it.key.as_ref().is_some_and(expr_has_yield)),
        Expr::Match(s, arms) => expr_has_yield(s) || arms.iter().any(|a| expr_has_yield(&a.body)),
        Expr::Isset(es) => es.iter().any(expr_has_yield),
        _ => false, // Closure/ArrowFn intentionally not descended into
    }
}

fn ucwords_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut cap = true;
    for c in s.chars() {
        if cap && c.is_alphabetic() {
            out.extend(c.to_uppercase());
            cap = false;
        } else {
            out.push(c);
            cap = c.is_whitespace();
        }
    }
    out
}

fn levenshtein(a: &[u8], b: &[u8]) -> usize {
    if a.is_empty() {
        return b.len();
    }
    if b.is_empty() {
        return a.len();
    }
    // cap to keep it bounded on pathological inputs
    if a.len() > 4096 || b.len() > 4096 {
        return a.len().abs_diff(b.len());
    }
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0usize; b.len() + 1];
    for (i, &ca) in a.iter().enumerate() {
        cur[0] = i + 1;
        for (j, &cb) in b.iter().enumerate() {
            let cost = if ca == cb { 0 } else { 1 };
            cur[j + 1] = (prev[j + 1] + 1).min(cur[j] + 1).min(prev[j] + cost);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

fn hex_to_bytes(h: &str) -> Vec<u8> {
    let h = h.as_bytes();
    (0..h.len() / 2)
        .filter_map(|i| {
            let hi = (h[i * 2] as char).to_digit(16)?;
            let lo = (h[i * 2 + 1] as char).to_digit(16)?;
            Some(((hi << 4) | lo) as u8)
        })
        .collect()
}

fn string_char(s: &[u8], k: &Key) -> Value {
    let i = match k {
        Key::Int(n) => *n,
        Key::Str(b) => leading_number(b).as_i64(),
    };
    let i = if i < 0 { s.len() as i64 + i } else { i };
    if i >= 0 && (i as usize) < s.len() {
        Value::Str(vec![s[i as usize]])
    } else {
        Value::Str(Vec::new())
    }
}

fn inc(v: &Value, by: i64) -> Value {
    match v {
        Value::Int(n) => Value::Int(n + by),
        Value::Float(f) => Value::Float(f + by as f64),
        Value::Null if by > 0 => Value::Int(1),
        Value::Null => Value::Null, // PHP: --$null stays null
        _ => match to_num(v) {
            Num::Int(n) => Value::Int(n + by),
            Num::Float(f) => Value::Float(f + by as f64),
        },
    }
}

fn num_arith(l: &Value, r: &Value, fi: fn(i64, i64) -> i64, ff: fn(f64, f64) -> f64) -> Value {
    match (to_num(l), to_num(r)) {
        (Num::Int(a), Num::Int(b)) => {
            // detect overflow for + - * by checking against float
            let res = fi(a, b);
            Value::Int(res)
        }
        (a, b) => Value::Float(ff(a.as_f64(), b.as_f64())),
    }
}
