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
    /// Active generator collection buffer (eager generators collect yields here).
    gen_buf: Option<Arr>,
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
class Exception {
    protected $message = "";
    protected $code = 0;
    protected $previous = null;
    public function __construct($message = "", $code = 0, $previous = null) {
        $this->message = $message; $this->code = $code; $this->previous = $previous;
    }
    public function getMessage() { return $this->message; }
    public function getCode() { return $this->code; }
    public function getPrevious() { return $this->previous; }
    public function getTrace() { return []; }
    public function getTraceAsString() { return "#0 {main}"; }
    public function getFile() { return ""; }
    public function getLine() { return 0; }
    public function __toString() { return $this->message; }
}
class ErrorException extends Exception {}
class Error extends Exception {}
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

interface DateTimeInterface {}
class DateTimeZone {
    public $name;
    public function __construct($name = "UTC") { $this->name = $name; }
    public function getName() { return $this->name; }
    public function getOffset($dt) { return 0; }
}
class DateTime implements DateTimeInterface {
    public $__ts;
    public function __construct($s = "now") { $this->__ts = strtotime($s); }
    public function format($fmt) { return date($fmt, $this->__ts); }
    public function getTimestamp() { return $this->__ts; }
    public function setTimestamp($ts) { $this->__ts = $ts; return $this; }
    public function setDate($y, $m, $d) { $this->__ts = mktime((int)date("H", $this->__ts), (int)date("i", $this->__ts), (int)date("s", $this->__ts), $m, $d, $y); return $this; }
    public function setTime($h, $i, $s = 0) { $this->__ts = mktime($h, $i, $s, (int)date("n", $this->__ts), (int)date("j", $this->__ts), (int)date("Y", $this->__ts)); return $this; }
    public function getTimezone() { return new DateTimeZone("UTC"); }
    public function setTimezone($tz) { return $this; }
    public function getOffset() { return 0; }
    public function add($iv) { $this->__ts = phargo_civil_add($this->__ts, $iv->y, $iv->m, $iv->d, $iv->h, $iv->i, $iv->s); return $this; }
    public function sub($iv) { $this->__ts = phargo_civil_add($this->__ts, -$iv->y, -$iv->m, -$iv->d, -$iv->h, -$iv->i, -$iv->s); return $this; }
    public function modify($s) { $this->__ts = __phargo_modify($this->__ts, $s); return $this; }
    public function diff($other) { return DateInterval::__fromArray(phargo_date_diff($this->__ts, $other->getTimestamp())); }
    public static function createFromFormat($fmt, $s, $tz = null) { return new DateTime($s); }
}
class DateTimeImmutable implements DateTimeInterface {
    public $__ts;
    public function __construct($s = "now") { $this->__ts = strtotime($s); }
    public function format($fmt) { return date($fmt, $this->__ts); }
    public function getTimestamp() { return $this->__ts; }
    public function getTimezone() { return new DateTimeZone("UTC"); }
    public function add($iv) { $n = clone $this; $n->__ts = phargo_civil_add($this->__ts, $iv->y, $iv->m, $iv->d, $iv->h, $iv->i, $iv->s); return $n; }
    public function sub($iv) { $n = clone $this; $n->__ts = phargo_civil_add($this->__ts, -$iv->y, -$iv->m, -$iv->d, -$iv->h, -$iv->i, -$iv->s); return $n; }
    public function modify($s) { $n = clone $this; $n->__ts = __phargo_modify($this->__ts, $s); return $n; }
    public function diff($other) { return DateInterval::__fromArray(phargo_date_diff($this->__ts, $other->getTimestamp())); }
    public static function createFromFormat($fmt, $s, $tz = null) { return new DateTimeImmutable($s); }
}
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
}
class ReflectionObject extends ReflectionClass {}
class ReflectionEnum extends ReflectionClass {}
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
    public function isStatic() { return false; }
    public function isPublic() { return true; }
    public function isAbstract() { return false; }
    public function setAccessible($a) {}
}
class ReflectionParameter {
    public $name; private $info;
    public function __construct($info) { $this->info = $info; $this->name = $info["name"]; }
    public function getName() { return $this->name; }
    public function isOptional() { return $this->info["optional"]; }
    public function isVariadic() { return $this->info["variadic"]; }
    public function hasType() { return $this->info["type"] !== null; }
    public function getType() { return $this->info["type"]; }
}
class ReflectionProperty {
    public $class; public $name;
    public function __construct($c, $n) { $this->class = is_object($c) ? get_class($c) : $c; $this->name = $n; }
    public function getName() { return $this->name; }
    public function getValue($obj = null) { $n = $this->name; return $obj->$n; }
    public function setValue($obj, $v) { $n = $this->name; $obj->$n = $v; }
    public function isPublic() { return true; }
    public function isStatic() { return false; }
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
}
class ReflectionNamedType { public $name; public function __construct($n) { $this->name = $n; } public function getName() { return $this->name; } public function allowsNull() { return false; } }
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
class ArrayObject implements ArrayAccess, IteratorAggregate, Countable {
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
    protected $__d = []; protected $__p = 0; protected $__lifo = false;
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
    public function rewind(): void { $this->__p = $this->__lifo ? count($this->__d) - 1 : 0; }
    public function valid(): bool { return $this->__p >= 0 && $this->__p < count($this->__d); }
    public function current(): mixed { return $this->__d[$this->__p]; }
    public function key(): mixed { return $this->__p; }
    public function next(): void { if ($this->__lifo) { $this->__p = $this->__p - 1; } else { $this->__p = $this->__p + 1; } }
}
class SplStack extends SplDoublyLinkedList { public function __construct() { $this->__lifo = true; } }
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
    private $__d = [];
    private function __top_index() { $best = 0; for ($i = 1; $i < count($this->__d); $i++) { if ($this->__d[$i][0] > $this->__d[$best][0]) { $best = $i; } } return $best; }
    public function insert($value, $priority) { $this->__d[] = [$priority, $value]; return true; }
    public function top() { return $this->__d[$this->__top_index()][1]; }
    public function extract() { if (count($this->__d) === 0) { return null; } $i = $this->__top_index(); $v = $this->__d[$i][1]; array_splice($this->__d, $i, 1); return $v; }
    public function count(): int { return count($this->__d); }
    public function isEmpty() { return count($this->__d) === 0; }
    public function rewind(): void {}
    public function valid(): bool { return count($this->__d) > 0; }
    public function current(): mixed { return $this->top(); }
    public function key(): mixed { return count($this->__d) - 1; }
    public function next(): void { $this->extract(); }
}
"##;

const STEP_LIMIT: u64 = 20_000_000;
/// Cap on single string allocations (concat, str_repeat) — stops memory bombs
/// from pathological corpus tests (huge `.=` / `str_repeat` / `range`).
const MAX_STR: usize = 64 * 1024 * 1024;
const MAX_RANGE: usize = 8_000_000;
/// Cap on total nodes in a single array value (memory-bomb guard).
const MAX_ARRAY_NODES: usize = 4_000_000;

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
            thrown: None,
            call_depth: 0,
            eval_depth: 0,
            cur_file: None,
            included: HashSet::new(),
            ob_stack: Vec::new(),
            gen_buf: None,
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

    /// Register the exception/error hierarchy (parsed from PRELUDE).
    fn load_prelude(&mut self) {
        if let Ok(toks) = super::lexer::Lexer::tokenize(PRELUDE) {
            if let Ok(stmts) = super::parser::Parser::parse(toks) {
                self.hoist(&stmts);
            }
        }
    }

    /// Run a parsed program and return everything it printed.
    pub fn run(program: &[Stmt]) -> R<Vec<u8>> {
        Self::run_with_path(program, None)
    }

    /// Like [`run`], but records the script's path so `__FILE__`/`__DIR__` and
    /// relative `include`/`require` resolve against it.
    pub fn run_with_path(program: &[Stmt], path: Option<PathBuf>) -> R<Vec<u8>> {
        let mut e = Eval::new();
        e.cur_file = path;
        e.load_prelude();
        e.hoist(program);
        match e.exec_block(program) {
            Ok(_) => Ok(e.out),
            Err(err) => {
                // `exit`/`die`: a normal halt, not an error.
                if err.0 == "__phargo_exit__" {
                    return Ok(e.out);
                }
                // An uncaught exception becomes a PHP fatal-error message in the
                // output (matching PHP). Other engine errors (step limit, unknown
                // function, parse) propagate with their message so the scoreboard
                // histogram stays useful.
                if let Some(exc) = e.thrown.take() {
                    let (cls, msg) = e.exception_info(&exc);
                    let file = e
                        .cur_file
                        .as_ref()
                        .map(|p| p.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    let s = format!(
                        "\nFatal error: Uncaught {cls}: {msg} in {file}:0\nStack trace:\n#0 {{main}}\n  thrown in {file} on line 0\n"
                    );
                    e.out.extend_from_slice(s.as_bytes());
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
        for s in stmts {
            match s {
                Stmt::Func(f) => {
                    self.funcs.insert(f.name.to_ascii_lowercase(), Rc::new(f.clone()));
                }
                Stmt::Class(c) => {
                    self.classes.insert(c.name.to_ascii_lowercase(), Rc::new(c.clone()));
                }
                _ => {}
            }
        }
    }

    fn vars(&mut self) -> &mut HashMap<String, Value> {
        self.scopes.last_mut().unwrap()
    }

    fn tick(&mut self) -> R<()> {
        self.steps += 1;
        if self.steps > STEP_LIMIT {
            return Err(RunError("step limit exceeded".into()));
        }
        Ok(())
    }

    // ---- statements -----------------------------------------------------
    fn exec_block(&mut self, stmts: &[Stmt]) -> R<Flow> {
        for s in stmts {
            match self.exec(s)? {
                Flow::Normal => {}
                other => return Ok(other),
            }
        }
        Ok(Flow::Normal)
    }

    fn exec(&mut self, s: &Stmt) -> R<Flow> {
        self.tick()?;
        match s {
            Stmt::InlineHtml(b) => self.out.extend_from_slice(b),
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
                self.funcs.insert(f.name.to_ascii_lowercase(), Rc::new(f.clone()));
            }
            Stmt::ConstDecl(decls) => {
                for (name, e) in decls {
                    let v = self.eval(e)?;
                    self.consts.insert(name.clone(), v);
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
            Stmt::Foreach { array, key, value, by_ref: _, body } => {
                let arr = self.eval(array)?;
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
                    Some(e) => self.eval(e)?,
                    None => Value::Null,
                };
                return Ok(Flow::Return(v));
            }
            Stmt::Unset(items) => {
                for it in items {
                    if let Expr::Var(name) = it {
                        self.vars().remove(name);
                    }
                }
            }
            Stmt::Global(names) => {
                // copy the global into the local scope (simplified: by value)
                for n in names {
                    let v = self.scopes[0].get(n).cloned().unwrap_or(Value::Null);
                    self.vars().insert(n.clone(), v);
                }
            }
            Stmt::Class(c) => {
                self.classes.insert(c.name.to_ascii_lowercase(), Rc::new(c.clone()));
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
            // not yet implemented in this increment — parsed but skipped
            Stmt::StaticVar(_)
            | Stmt::Namespace { .. }
            | Stmt::Use(_)
            | Stmt::Declare => {}
        }
        Ok(Flow::Normal)
    }

    fn unwind_break(&self, n: u32) -> R<Flow> {
        Ok(if n > 1 { Flow::Break(n - 1) } else { Flow::Normal })
    }

    /// One foreach iteration: bind key/value, run the body. Returns `Some(flow)`
    /// if the loop must stop (break/return propagation), `None` to continue.
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
                    let val = self.eval(&it.value)?;
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
            Expr::Var(name) => self.vars().get(name).cloned().unwrap_or(Value::Null),
            Expr::ConstFetch(name) => self.const_fetch(name),
            Expr::MagicConst(name) => match name.to_ascii_uppercase().as_str() {
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
                "__CLASS__" => Value::Str(self.current_class.clone().unwrap_or_default().into_bytes()),
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
                // references not modeled yet — behave as a value assignment
                let v = self.eval(rhs)?;
                self.assign_to(lhs, v.clone())?;
                v
            }
            Expr::AssignOp(op, lhs, rhs) => {
                // `$v .= expr` in place: append to the existing string instead of
                // cloning it each time (avoids O(n^2) growth on `.=` loops).
                if *op == BinOp::Concat {
                    if let Expr::Var(name) = &**lhs {
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
                let nv = self.apply_bin(*op, &cur, &rv);
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
                let mut all = true;
                for it in items {
                    if matches!(self.eval(it)?, Value::Null) {
                        all = false;
                        break;
                    }
                }
                Value::Bool(all)
            }
            Expr::Empty(e) => Value::Bool(!to_bool(&self.eval(e)?)),
            Expr::ErrorSuppress(e) => self.eval(e).unwrap_or(Value::Null),
            Expr::Print(e) => {
                let v = self.eval(e)?;
                self.out.extend_from_slice(&to_bytes(&v));
                Value::Int(1)
            }
            Expr::Cast(ct, e) => {
                let v = self.eval(e)?;
                self.cast(*ct, v)
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
                result.ok_or_else(|| RunError("UnhandledMatchError".into()))?
            }
            Expr::New(class, args) => {
                let cname = self.resolve_class_name(class)?;
                let argv = self.eval_args(args)?;
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
                        rc.borrow().get(&pname).cloned().unwrap_or(Value::Null)
                    }
                    _ => Value::Null,
                }
            }
            Expr::MethodCall(obj, name, args, nullsafe) => {
                let o = self.eval(obj)?;
                if *nullsafe && matches!(o, Value::Null) {
                    return Ok(Value::Null);
                }
                let mname = self.prop_name_str(name)?;
                let argv = self.eval_args(args)?;
                self.call_method(o, &mname, argv)?
            }
            Expr::StaticCall(class, name, args) => {
                let cname = self.resolve_class_name(class)?;
                let mname = self.prop_name_str(name)?;
                let argv = self.eval_args(args)?;
                // `parent::`/`self::` keep the current $this if present
                let this = self.vars().get("this").cloned();
                self.call_static(&cname, &mname, argv, this)?
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
                self.static_props
                    .get(&(cname.to_ascii_lowercase(), name.clone()))
                    .cloned()
                    .unwrap_or(Value::Null)
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
                        if let Some(buf) = self.gen_buf.as_mut() {
                            for (k, v) in a.entries {
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
                        if let (Some(buf), Value::Array(a)) = (self.gen_buf.as_mut(), drained) {
                            for (_, v) in a.entries {
                                buf.push(v);
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
                    let v = self.vars().get(&u.name).cloned().unwrap_or(Value::Null);
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
                let captures: Vec<(String, Value)> =
                    self.vars().iter().map(|(k, v)| (k.clone(), v.clone())).collect();
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
                let lv = self.eval(l)?;
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
        Ok(self.apply_bin(op, &lv, &rv))
    }

    fn apply_bin(&self, op: BinOp, l: &Value, r: &Value) -> Value {
        use BinOp::*;
        match op {
            Add => {
                if let (Value::Array(a), Value::Array(b)) = (l, r) {
                    let mut out = a.clone();
                    for (k, v) in &b.entries {
                        if out.get(k).is_none() {
                            out.insert(k.clone(), v.clone());
                        }
                    }
                    return Value::Array(out);
                }
                num_arith(l, r, |a, b| a.wrapping_add(b), |a, b| a + b)
            }
            Sub => num_arith(l, r, |a, b| a.wrapping_sub(b), |a, b| a - b),
            Mul => num_arith(l, r, |a, b| a.wrapping_mul(b), |a, b| a * b),
            Div => {
                let rf = to_f64(r);
                if rf == 0.0 {
                    return Value::Bool(false); // div-by-zero (legacy-ish); real PHP throws
                }
                match (to_num(l), to_num(r)) {
                    (Num::Int(a), Num::Int(b)) if b != 0 && a % b == 0 => Value::Int(a / b),
                    _ => Value::Float(to_f64(l) / rf),
                }
            }
            Mod => {
                let b = to_i64(r);
                if b == 0 {
                    Value::Bool(false)
                } else {
                    Value::Int(to_i64(l).wrapping_rem(b))
                }
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
            BitAnd => Value::Int(to_i64(l) & to_i64(r)),
            BitOr => Value::Int(to_i64(l) | to_i64(r)),
            BitXor => Value::Int(to_i64(l) ^ to_i64(r)),
            Shl => Value::Int(to_i64(l).wrapping_shl(to_i64(r) as u32)),
            Shr => Value::Int(to_i64(l).wrapping_shr(to_i64(r) as u32)),
            Xor => Value::Bool(to_bool(l) ^ to_bool(r)),
            // logicals handled in `binary`
            And | Or | Coalesce => Value::Null,
        }
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
                other => {
                    let mut a = Arr::new();
                    a.push(other);
                    Value::Array(a)
                }
            },
            CastType::Object => match v {
                Value::Object(_) | Value::Closure(_) => v,
                Value::Array(a) => {
                    let mut o = Obj { class: "stdClass".into(), props: Vec::new() };
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
                    let o = Rc::new(RefCell::new(Obj { class: "stdClass".into(), props: Vec::new() }));
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
        // ArrayAccess: `$expr[$k]` where the base is an object → offsetGet($k).
        // (Single level; deeper chains fall through to the array path below.)
        let base_obj = match base {
            Expr::Var(name) => matches!(self.vars().get(name), Some(Value::Object(_))),
            Expr::Prop(..) | Expr::Index(..) | Expr::MethodCall(..) | Expr::Call(..) | Expr::StaticCall(..) => {
                false // resolved on demand below to avoid double-eval of arrays
            }
            _ => false,
        };
        if base_obj {
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
        // Navigate by reference (keys already evaluated — no &mut self needed here).
        let scope = self.scopes.last().unwrap();
        let mut v = match scope.get(&name) {
            Some(v) => v,
            None => return Ok(Value::Null),
        };
        for k in &keys {
            match v {
                Value::Array(a) => {
                    v = match a.get(k) {
                        Some(x) => x,
                        None => return Ok(Value::Null),
                    };
                }
                Value::Str(s) => return Ok(string_char(s, k)),
                _ => return Ok(Value::Null),
            }
        }
        Ok(v.clone())
    }

    fn index_get_key(&self, base: &Value, k: &Key) -> Value {
        match base {
            Value::Array(a) => a.get(k).cloned().unwrap_or(Value::Null),
            Value::Str(s) => string_char(s, k),
            _ => Value::Null,
        }
    }

    fn index_get(&self, base: &Value, idx: &Value) -> Value {
        match base {
            Value::Array(a) => a.get(&Arr::norm_key(idx)).cloned().unwrap_or(Value::Null),
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

    fn const_fetch(&self, name: &Name) -> Value {
        let n = name.last();
        if let Some(v) = self.consts.get(n) {
            return v.clone();
        }
        php_const(n).unwrap_or_else(|| {
            // unknown bareword → its own name as a string (PHP 7 behavior-ish)
            Value::Str(n.as_bytes().to_vec())
        })
    }

    // ---- assignment targets --------------------------------------------
    fn assign_to(&mut self, target: &Expr, val: Value) -> R<()> {
        // Guard against pathological array explosion (e.g. self-referential
        // value-copy where references aren't modeled yet).
        if matches!(val, Value::Array(_)) && value_size(&val, MAX_ARRAY_NODES) > MAX_ARRAY_NODES {
            return Err(self.throw_error("Error", "Allocated array exceeds memory limit"));
        }
        match target {
            Expr::Var(name) => {
                self.vars().insert(name.clone(), val);
            }
            Expr::Index(base, idx) => {
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
                    rc.borrow_mut().set(&pname, val);
                }
            }
            Expr::StaticProp(class, name) => {
                let cname = self.resolve_class_name(class)?;
                self.static_props
                    .insert((cname.to_ascii_lowercase(), name.clone()), val);
            }
            // list/array destructuring: [$a, $b] = ...  and  list($a, $b) = ...
            Expr::Array(_) | Expr::List(_) => {
                self.destructure(target, val)?;
            }
            _ => {}
        }
        Ok(())
    }

    fn assign_index(&mut self, base: &Expr, key: Option<Key>, val: Value) -> R<()> {
        // Read-modify-write the base container. Only simple `$var[...]` (one level)
        // and nested `$var[a][b]` are handled here.
        match base {
            Expr::Var(name) => {
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
                // nested: evaluate current, mutate, write back
                let ikey = match iidx {
                    Some(i) => Some(Arr::norm_key(&self.eval(i)?)),
                    None => None,
                };
                let mut cur = self.eval(base).unwrap_or(Value::Array(Arr::new()));
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
                    let v = a.get(&key).cloned().unwrap_or(Value::Null);
                    self.assign_to(&item.value, v)?;
                } else {
                    idx += 1;
                }
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
            let name = n.last().to_ascii_lowercase();
            let argv = self.eval_args(args)?;
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
                    let (count, matches) = self.preg_run(&pat, &subj, all);
                    if args.len() > 2 {
                        self.assign_to(&args[2].value, matches)?;
                    }
                    return Ok(Value::Int(count));
                }
                "array_push" | "array_pop" | "array_shift" | "array_unshift" | "sort" | "rsort"
                | "asort" | "arsort" | "ksort" | "krsort" | "usort" | "uasort" | "uksort"
                | "array_splice" | "shuffle"
                    if !args.is_empty() =>
                {
                    return self.array_byref(&name, args, &argv);
                }
                _ => {}
            }
            if let Some(f) = self.funcs.get(&name).cloned() {
                return self.call_user(&f, argv);
            }
            return self.builtin(&name, argv);
        }
        // dynamic callee: $f(...), expr(...) — evaluate to a callable value
        let cv = self.eval(callee)?;
        let argv = self.eval_args(args)?;
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
        let toks = super::lexer::Lexer::tokenize(bytes)
            .map_err(|e| RunError(format!("Parse error: {}", e.msg)))?;
        let ast = super::parser::Parser::parse(toks)
            .map_err(|e| RunError(format!("Parse error: {}", e.msg)))?;
        let prev_file = std::mem::replace(&mut self.cur_file, path);
        self.hoist(&ast);
        let r = self.exec_block(&ast);
        self.cur_file = prev_file;
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
    fn preg_run(&self, pat: &[u8], subj: &[u8], all: bool) -> (i64, Value) {
        let pattern = String::from_utf8_lossy(pat).into_owned();
        let rx = match crate::rx_compile(&pattern) {
            Some(r) => r,
            None => return (0, Value::Bool(false)),
        };
        let text: Vec<char> = String::from_utf8_lossy(subj).chars().collect();
        let mut steps = 0usize;
        let grp = |slots: &[usize], g: usize| Value::Str(crate::rx_group_str(&text, slots, g).into_bytes());
        if !all {
            match rx.exec(&text, 0, &mut steps) {
                Some(slots) => {
                    let mut m = Arr::new();
                    for g in 0..=rx.ngroups {
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
            let mut start = 0;
            while let Some(slots) = rx.exec(&text, start, &mut steps) {
                let (ms, me) = (slots[0], slots[1]);
                sets.push(slots);
                start = if me > ms { me } else { me + 1 };
                if start > text.len() {
                    break;
                }
            }
            let mut result = Arr::new();
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
                    self.call_user(&f, args)
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
                        self.call_static(&cn, &mname, args, None)
                    }
                    _ => Ok(Value::Null),
                }
            }
            _ => Ok(Value::Null),
        }
    }

    fn call_closure(&mut self, c: &ClosureVal, args: Vec<Value>) -> R<Value> {
        self.enter_call()?;
        let mut scope = HashMap::new();
        for (k, v) in &c.captures {
            scope.insert(k.clone(), v.clone());
        }
        if let Some(t) = &c.bound_this {
            scope.insert("this".to_string(), t.clone());
        }
        let r = match &c.kind {
            ClosureKind::Full(f) => {
                self.bind_params(&mut scope, &f.params, &args)?;
                self.scopes.push(scope);
                let r = self.run_fn_body(&f.body);
                self.scopes.pop();
                r
            }
            ClosureKind::Arrow(f) => {
                self.bind_params(&mut scope, &f.params, &args)?;
                self.scopes.push(scope);
                let r = self.eval(&f.body);
                self.scopes.pop();
                r
            }
        };
        self.call_depth -= 1;
        r
    }

    /// Run a function/method body that has already had its scope pushed. If the
    /// body contains `yield`, run it as an (eager) generator and return a
    /// Generator object; otherwise return the `return` value.
    fn run_fn_body(&mut self, body: &[Stmt]) -> R<Value> {
        if has_yield(body) {
            let prev = self.gen_buf.take();
            self.gen_buf = Some(Arr::new());
            let r = self.exec_block(body);
            let buf = self.gen_buf.take().unwrap_or_default();
            self.gen_buf = prev;
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
        let o = Rc::new(RefCell::new(Obj { class: "Generator".into(), props: Vec::new() }));
        {
            let mut b = o.borrow_mut();
            b.set("__d", Value::Array(buf));
            b.set("__k", Value::Array(karr));
            b.set("__p", Value::Int(0));
            b.set("__ret", ret);
        }
        Value::Object(o)
    }

    fn call_user(&mut self, f: &FuncDecl, args: Vec<Value>) -> R<Value> {
        self.enter_call()?;
        let mut scope = HashMap::new();
        for (i, p) in f.params.iter().enumerate() {
            if p.variadic {
                let mut rest = Arr::new();
                for v in args.iter().skip(i) {
                    rest.push(v.clone());
                }
                scope.insert(p.name.clone(), Value::Array(rest));
                break;
            }
            let v = match args.get(i) {
                Some(v) => v.clone(),
                None => match &p.default {
                    Some(d) => self.eval(d)?,
                    None => Value::Null,
                },
            };
            scope.insert(p.name.clone(), v);
        }
        self.scopes.push(scope);
        let r = self.run_fn_body(&f.body);
        self.scopes.pop();
        self.call_depth -= 1;
        r
    }
}

// ---- objects & classes -------------------------------------------------
impl Eval {
    fn find_class(&self, name: &str) -> Option<Rc<ClassDecl>> {
        self.classes.get(&name.to_ascii_lowercase()).cloned()
    }

    /// Resolve a class reference expression to a class name.
    fn resolve_class_name(&mut self, e: &Expr) -> R<String> {
        match e {
            Expr::ConstFetch(n) => {
                let last = n.last();
                match last.to_ascii_lowercase().as_str() {
                    "self" | "static" => Ok(self
                        .current_class
                        .clone()
                        .unwrap_or_else(|| last.to_string())),
                    "parent" => {
                        let cur = self.current_class.clone().unwrap_or_default();
                        Ok(self
                            .find_class(&cur)
                            .and_then(|c| c.parent.as_ref().map(|p| p.last().to_string()))
                            .unwrap_or(cur))
                    }
                    _ => Ok(last.to_string()),
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
        let mut out = Vec::new();
        for a in args {
            if a.name.as_deref() == Some("...") {
                continue;
            }
            if a.spread {
                if let Value::Array(arr) = self.eval(&a.value)? {
                    for (_, v) in arr.entries {
                        out.push(v);
                    }
                }
            } else {
                out.push(self.eval(&a.value)?);
            }
        }
        Ok(out)
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
            cur = c.parent.as_ref().and_then(|p| self.find_class(p.last()));
        }
        out
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
    fn find_method(&self, class: &str, method: &str) -> Option<(String, MethodDecl)> {
        let m = method.to_ascii_lowercase();
        for c in self.ancestry(class) {
            // traits first (declared in the class), then own methods
            for t in &c.uses_traits {
                if let Some(tc) = self.find_class(t.last()) {
                    if let Some(md) = tc.methods.iter().find(|x| x.name.to_ascii_lowercase() == m) {
                        return Some((c.name.clone(), md.clone()));
                    }
                }
            }
            if let Some(md) = c.methods.iter().find(|x| x.name.to_ascii_lowercase() == m) {
                return Some((c.name.clone(), md.clone()));
            }
        }
        None
    }

    fn instantiate(&mut self, class: &str, args: Vec<Value>) -> R<Value> {
        let decl = match self.find_class(class) {
            Some(d) => d,
            None => return Err(RunError(format!("class {class} not found"))),
        };
        let obj = Rc::new(RefCell::new(Obj { class: decl.name.clone(), props: Vec::new() }));
        // initialize declared (instance) properties from the whole hierarchy,
        // base-most first so overrides win.
        let chain = self.ancestry(class);
        for c in chain.iter().rev() {
            for p in &c.props {
                if p.is_static {
                    continue;
                }
                let v = match &p.default {
                    Some(d) => self.eval(d)?,
                    None => Value::Null,
                };
                obj.borrow_mut().set(&p.name, v);
            }
        }
        let ov = Value::Object(obj);
        // constructor
        if self.find_method(class, "__construct").is_some() {
            self.call_method(ov.clone(), "__construct", args)?;
        }
        Ok(ov)
    }

    fn call_method(&mut self, recv: Value, method: &str, args: Vec<Value>) -> R<Value> {
        let class = match &recv {
            Value::Object(rc) => rc.borrow().class.clone(),
            _ => return Ok(Value::Null),
        };
        let (decl_class, m) = match self.find_method(&class, method) {
            Some(x) => x,
            None => {
                // __call magic fallback
                if let Some((dc, _)) = self.find_method(&class, "__call") {
                    let mut a = Arr::new();
                    for v in args {
                        a.push(v);
                    }
                    let cargs = vec![Value::Str(method.as_bytes().to_vec()), Value::Array(a)];
                    return self.invoke_method(recv, &dc, &self.find_method(&class, "__call").unwrap().1.clone(), cargs);
                }
                return Err(RunError(format!("call to undefined method {class}::{method}()")));
            }
        };
        self.invoke_method(recv, &decl_class, &m, args)
    }

    fn invoke_method(
        &mut self,
        recv: Value,
        decl_class: &str,
        m: &MethodDecl,
        args: Vec<Value>,
    ) -> R<Value> {
        let body = match &m.body {
            Some(b) => b.clone(),
            None => return Ok(Value::Null),
        };
        self.enter_call()?;
        let mut scope = HashMap::new();
        if !m.is_static {
            scope.insert("this".to_string(), recv.clone());
        }
        self.bind_params(&mut scope, &m.params, &args)?;
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
        let prev_class = self.current_class.replace(decl_class.to_string());
        self.scopes.push(scope);
        let r = self.run_fn_body(&body);
        self.scopes.pop();
        self.current_class = prev_class;
        self.call_depth -= 1;
        r
    }

    fn call_static(
        &mut self,
        class: &str,
        method: &str,
        args: Vec<Value>,
        this: Option<Value>,
    ) -> R<Value> {
        let (decl_class, m) = match self.find_method(class, method) {
            Some(x) => x,
            None => return Err(RunError(format!("call to undefined method {class}::{method}()"))),
        };
        let body = match &m.body {
            Some(b) => b.clone(),
            None => return Ok(Value::Null),
        };
        self.enter_call()?;
        let mut scope = HashMap::new();
        // a non-static method reached via parent::/self:: keeps $this
        if !m.is_static {
            if let Some(t) = this {
                scope.insert("this".to_string(), t);
            }
        }
        self.bind_params(&mut scope, &m.params, &args)?;
        let prev_class = self.current_class.replace(decl_class.clone());
        self.scopes.push(scope);
        let r = self.run_fn_body(&body);
        self.scopes.pop();
        self.current_class = prev_class;
        self.call_depth -= 1;
        r
    }

    fn bind_params(
        &mut self,
        scope: &mut HashMap<String, Value>,
        params: &[Param],
        args: &[Value],
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
            let v = match args.get(i) {
                Some(v) => v.clone(),
                None => match &p.default {
                    Some(d) => self.eval(d)?,
                    None => Value::Null,
                },
            };
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
        // enum case?
        if let Some(c) = self.find_class(class) {
            if c.kind == ClassKind::Enum {
                if c.cases.iter().any(|e| e.name == name) {
                    // model an enum case as an object with `name` (+ `value`)
                    let obj = Rc::new(RefCell::new(Obj { class: c.name.clone(), props: Vec::new() }));
                    obj.borrow_mut().set("name", Value::Str(name.as_bytes().to_vec()));
                    if let Some(ec) = c.cases.iter().find(|e| e.name == name) {
                        if let Some(v) = &ec.value {
                            let val = self.eval(v)?;
                            obj.borrow_mut().set("value", val);
                        }
                    }
                    return Ok(Value::Object(obj));
                }
            }
        }
        for c in self.ancestry(class) {
            if let Some(cc) = c.consts.iter().find(|x| x.name == name) {
                return self.eval(&cc.value.clone());
            }
            // interface constants
            for i in &c.interfaces {
                if let Some(ic) = self.find_class(i.last()) {
                    if let Some(cc) = ic.consts.iter().find(|x| x.name == name) {
                        return self.eval(&cc.value.clone());
                    }
                }
            }
        }
        Err(RunError(format!("undefined constant {class}::{name}")))
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
                    let id = Rc::as_ptr(&rc) as *const () as usize as i64;
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
                    var_dump(v, 0, &mut s);
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
            "is_object" => Value::Bool(matches!(a(0), Value::Object(_) | Value::Closure(_))),
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
            "sqrt" => Value::Float(to_f64(&a(0)).sqrt()),
            "pow" => self.apply_bin(BinOp::Pow, &a(0), &a(1)),
            "intdiv" => {
                let d = to_i64(&a(1));
                if d == 0 {
                    return Err(self.throw_error("DivisionByZeroError", "Division by zero"));
                }
                Value::Int(to_i64(&a(0)) / d)
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
            "trim" => Value::Str(trim_bytes(&to_bytes(&a(0)), true, true)),
            "ltrim" => Value::Str(trim_bytes(&to_bytes(&a(0)), true, false)),
            "rtrim" | "chop" => Value::Str(trim_bytes(&to_bytes(&a(0)), false, true)),
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
            "str_replace" => {
                let search = to_bytes(&a(0));
                let replace = to_bytes(&a(1));
                let subject = to_bytes(&a(2));
                Value::Str(replace_bytes(&subject, &search, &replace))
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
                let mut out = Arr::new();
                if let Value::Array(arr) = a(0) {
                    for (k, v) in arr.entries {
                        let keep = if matches!(cb, Value::Null) {
                            to_bool(&v)
                        } else {
                            to_bool(&self.call_value(cb.clone(), vec![v.clone()])?)
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
                Value::Object(rc) => Value::Str(rc.borrow().class.clone().into_bytes()),
                _ => match &self.current_class {
                    Some(c) => Value::Str(c.clone().into_bytes()),
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
                Value::Bool(self.find_class(&n).is_some())
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
                let mut arr = Arr::new();
                let mut seen = HashSet::new();
                for c in self.ancestry(&cn) {
                    for m in &c.methods {
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
                    for p in ps {
                        let mut info = Arr::new();
                        info.insert(Key::Str(b"name".to_vec()), Value::Str(p.name.as_bytes().to_vec()));
                        info.insert(
                            Key::Str(b"optional".to_vec()),
                            Value::Bool(p.default.is_some() || p.variadic),
                        );
                        info.insert(Key::Str(b"variadic".to_vec()), Value::Bool(p.variadic));
                        info.insert(
                            Key::Str(b"type".to_vec()),
                            match &p.type_hint {
                                Some(t) => Value::Str(t.as_bytes().to_vec()),
                                None => Value::Null,
                            },
                        );
                        arr.push(Value::Array(info));
                    }
                }
                Value::Array(arr)
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
                Value::Str(crate::php_date(&fmt, ts).into_bytes())
            }
            "strftime" | "gmstrftime" => {
                let fmt = String::from_utf8_lossy(&to_bytes(&a(0))).into_owned();
                let ts = if args.len() > 1 { to_i64(&a(1)) } else { crate::now_unix() };
                Value::Str(crate::php_strftime(&fmt, ts).into_bytes())
            }
            "mktime" | "gmmktime" => {
                let now = crate::now_unix();
                let (cy, cm, cd) = crate::civil_from_days(now.div_euclid(86400));
                let secs = now.rem_euclid(86400);
                let g = |i: usize, dflt: i64| if args.len() > i { to_i64(&a(i)) } else { dflt };
                Value::Int(crate::make_ts(
                    g(0, secs / 3600),
                    g(1, (secs % 3600) / 60),
                    g(2, secs % 60),
                    g(3, cm),
                    g(4, cd),
                    g(5, cy),
                ))
            }
            "strtotime" => {
                let s = String::from_utf8_lossy(&to_bytes(&a(0))).into_owned();
                let base = if args.len() > 1 { to_i64(&a(1)) } else { crate::now_unix() };
                match crate::php_strtotime(&s, base) {
                    Some(t) => Value::Int(t),
                    None => Value::Bool(false),
                }
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
            "preg_replace" => {
                let subject = String::from_utf8_lossy(&to_bytes(&a(2))).into_owned();
                let limit = if args.len() > 3 { to_i64(&a(3)) } else { -1 };
                let pats: Vec<Vec<u8>> = match a(0) {
                    Value::Array(arr) => arr.entries.into_iter().map(|(_, v)| to_bytes(&v)).collect(),
                    v => vec![to_bytes(&v)],
                };
                let rep_is_arr = matches!(a(1), Value::Array(_));
                let reps: Vec<Vec<u8>> = match a(1) {
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
                Value::Str(result.into_bytes())
            }
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
                let no_empty = to_i64(&a(3)) & 1 != 0;
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
                php_serialize(&a(0), &mut out, 0);
                Value::Str(out)
            }
            "unserialize" => {
                let bytes = to_bytes(&a(0));
                let mut pos = 0;
                php_unserialize(&bytes, &mut pos, 0).unwrap_or(Value::Bool(false))
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
                let ts = to_i64(&a(0));
                let days0 = ts.div_euclid(86400);
                let secs0 = ts.rem_euclid(86400);
                let (y, mo, d) = crate::civil_from_days(days0);
                let (dy, dm, dd) = (to_i64(&a(1)), to_i64(&a(2)), to_i64(&a(3)));
                let (dh, di, ds) = (to_i64(&a(4)), to_i64(&a(5)), to_i64(&a(6)));
                let total_months = (y * 12 + (mo - 1)) + dy * 12 + dm;
                let ny = total_months.div_euclid(12);
                let nmo = total_months.rem_euclid(12) + 1;
                let nday = d.min(crate::days_in_month(ny, nmo));
                let base = crate::days_from_civil(ny, nmo, nday) * 86400 + secs0;
                Value::Int(base + dd * 86400 + dh * 3600 + di * 60 + ds)
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
                let days0 = ts.div_euclid(86400);
                let secs0 = ts.rem_euclid(86400);
                let (y, mo, d) = crate::civil_from_days(days0);
                let total_months = (y * 12 + (mo - 1)) + dy * 12 + dm;
                let ny = total_months.div_euclid(12);
                let nmo = total_months.rem_euclid(12) + 1;
                let nday = d.min(crate::days_in_month(ny, nmo));
                let base = crate::days_from_civil(ny, nmo, nday) * 86400 + secs0;
                Value::Int(base + dd * 86400 + dh * 3600 + di * 60 + ds)
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
            "putenv" | "set_time_limit" | "ignore_user_abort" | "setlocale" | "extension_loaded" => {
                Value::Bool(false)
            }
            "ini_get" | "ini_set" => Value::Bool(false),
            "error_reporting" => Value::Int(0),
            "set_error_handler" | "restore_error_handler" | "set_exception_handler"
            | "restore_exception_handler" | "error_clear_last" | "debug_print_backtrace"
            | "gc_enable" | "gc_disable" | "header" | "clearstatcache" | "usleep" | "sleep" => {
                Value::Null
            }
            "trigger_error" | "spl_autoload_register" | "spl_autoload_unregister"
            | "date_default_timezone_set" | "assert" | "gc_enabled" | "headers_sent" => {
                Value::Bool(true)
            }
            "date_default_timezone_get" => Value::Str(b"UTC".to_vec()),
            "debug_backtrace" => Value::Array(Arr::new()),
            "gc_collect_cycles" | "http_response_code" | "getmypid" | "hrtime" => Value::Int(0),
            "memory_get_usage" | "memory_get_peak_usage" => Value::Int(2_000_000),
            "php_sapi_name" => Value::Str(b"cli".to_vec()),
            "phpversion" => Value::Str(b"8.3.0".to_vec()),
            "php_uname" => Value::Str(b"Linux".to_vec()),
            "error_get_last" => Value::Null,
            _ => return Err(RunError(format!("unknown function {name}()"))),
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
            let spec = String::from_utf8_lossy(&fmt[spec_start..i]).into_owned();
            i += 1;
            let arg = args.get(ai).cloned().unwrap_or(Value::Null);
            ai += 1;
            let piece = format_spec(conv, &spec, &arg);
            out.extend_from_slice(&piece);
        }
        out
    }
}

fn is_known_builtin(n: &str) -> bool {
    matches!(
        n,
        "strlen" | "count" | "var_dump" | "print_r" | "implode" | "explode" | "sprintf"
            | "printf" | "in_array" | "array_keys" | "array_values" | "array_merge" | "range"
    )
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
fn var_dump(v: &Value, indent: usize, out: &mut String) {
    var_dump_seen(v, indent, out, &mut Vec::new());
}

/// `seen` holds the Rc addresses of objects on the current path, so circular
/// object graphs print `*RECURSION*` instead of recursing into a memory bomb.
fn var_dump_seen(v: &Value, indent: usize, out: &mut String, seen: &mut Vec<usize>) {
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
        Value::Float(f) => out.push_str(&format!("{pad}float({})\n", format_float(*f))),
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
                var_dump_seen(val, indent + 1, out, seen);
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
            out.push_str(&format!("{pad}object({})#1 ({}) {{\n", ob.class, ob.props.len()));
            seen.push(id);
            for (k, val) in &ob.props {
                out.push_str(&format!("{pad}  [\"{k}\"]=>\n"));
                var_dump_seen(val, indent + 1, out, seen);
            }
            seen.pop();
            out.push_str(&format!("{pad}}}\n"));
        }
        Value::Closure(_) => out.push_str(&format!("{pad}object(Closure)#1 (0) {{\n{pad}}}\n")),
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
        "E_ALL" => Int(32767),
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
        // htmlspecialchars / ent
        "ENT_QUOTES" => Int(3),
        "ENT_COMPAT" => Int(2),
        "ENT_NOQUOTES" => Int(0),
        "ENT_HTML401" => Int(0),
        "ENT_HTML5" => Int(48),
        // json
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
        _ => return None,
    })
}

// ---- serialize / unserialize (byte-based, for the v2 Value) -------------
fn php_serialize(v: &Value, out: &mut Vec<u8>, depth: usize) {
    // Depth cap + a hard output-size cap: this serializer doesn't emit `r:`/`R:`
    // back-references, so shared/cyclic object graphs would otherwise blow up
    // exponentially (bug36424).
    if depth > 256 || out.len() > MAX_STR {
        out.extend_from_slice(b"N;");
        return;
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
            for (k, val) in &a.entries {
                match k {
                    Key::Int(n) => out.extend_from_slice(format!("i:{n};").as_bytes()),
                    Key::Str(s) => {
                        out.extend_from_slice(format!("s:{}:\"", s.len()).as_bytes());
                        out.extend_from_slice(s);
                        out.extend_from_slice(b"\";");
                    }
                }
                php_serialize(val, out, depth + 1);
            }
            out.push(b'}');
        }
        Value::Object(rc) => {
            let o = rc.borrow();
            out.extend_from_slice(
                format!("O:{}:\"{}\":{}:{{", o.class.len(), o.class, o.props.len()).as_bytes(),
            );
            for (name, val) in &o.props {
                out.extend_from_slice(format!("s:{}:\"{}\";", name.len(), name).as_bytes());
                php_serialize(val, out, depth + 1);
            }
            out.push(b'}');
        }
        Value::Closure(_) => out.extend_from_slice(b"N;"),
    }
}

fn ser_float(f: f64) -> String {
    if f.is_nan() {
        "NAN".into()
    } else if f.is_infinite() {
        if f < 0.0 { "-INF".into() } else { "INF".into() }
    } else {
        format!("{f}") // Rust's default is shortest round-trip
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
            let obj = Rc::new(RefCell::new(Obj { class, props: Vec::new() }));
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
        let o = Rc::new(RefCell::new(Obj { class: "stdClass".into(), props: Vec::new() }));
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
