# Autoload to manage thread pool

@tool
extends Node

signal _processing()

const Callable = preload("Callable.gd")
const RetCallHandle = preload("RetCallHandle.gd")

var handles: Array = []

var _queue: Array = []
var _lock: Mutex = Mutex.new()
var _sema: Semaphore = Semaphore.new()

var _ret_queue: Array = []
var _ret_lock: Mutex = Mutex.new()

func _enter_tree():
	var ThreadRunner = load("res://addons/instance_threadpool/ThreadRunner.gd")

	# We have to halve the thread count, otherwise lag will be unbearable
	var thread_count: int = ceil(OS.get_processor_count() / 2)

	for i in range(thread_count):
		var t = ThreadRunner.new()
		if t._start(self) != OK:
			break
		handles.append(t)

func _exit_tree():
	for i in range(len(handles)):
		_sema.post()
	while len(handles) != 0:
		var t = handles.pop_back()
		t._stop()
	_handle_ret()

func _process(_delta):
	_handle_ret()

# Queues call to main thread.
# Useful for host bindings to call to scene tree.
func queue_call_main(
	obj: Object,
	method: String,
	args: Array
):
	false # _ret_lock.lock() # TODOConverter40, Image no longer requires locking, `false` helps to not break one line if/else, so it can freely be removed
	_ret_queue.push_back(RetCallHandle.new(obj, method, args))
	false # _ret_lock.unlock() # TODOConverter40, Image no longer requires locking, `false` helps to not break one line if/else, so it can freely be removed

# Queues call to thread pool.
func queue_call_threadpool(
	obj: Object,
	method: String,
	args: Array
):
	_push_queue(RetCallHandle.new(obj, method, args))

func _push_queue(v):
	false # _lock.lock() # TODOConverter40, Image no longer requires locking, `false` helps to not break one line if/else, so it can freely be removed
	_queue.push_back(v)
	false # _lock.unlock() # TODOConverter40, Image no longer requires locking, `false` helps to not break one line if/else, so it can freely be removed
	_sema.post()

func _handle_ret():
	var q: Array
	false # _ret_lock.lock() # TODOConverter40, Image no longer requires locking, `false` helps to not break one line if/else, so it can freely be removed
	q = _ret_queue
	_ret_queue = []
	false # _ret_lock.unlock() # TODOConverter40, Image no longer requires locking, `false` helps to not break one line if/else, so it can freely be removed

	# We have to do 2 things:
	# 1. Call every callable in queue
	# 2. Not drop any object mid-flight
	# With that, we used signal to isolate each call.
	# So even if one errors, it does not affect other calls.
	# And we need to keep the queue array intact.
	for v in q:
		connect("_processing",Callable(v,"_call").bind(),CONNECT_ONE_SHOT)

	emit_signal("_processing")
