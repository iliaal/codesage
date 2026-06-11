# TODO: replace with async impl
def comment_todo():
    pass


# FIXME: race condition between read and write
async def async_fixme():
    pass


def docstring_why():
    """WHY: bypass cache for hot path"""
    return True


class DocstringNote:
    """NOTE: thread-safe, see lock in __init__"""

    def __init__(self):
        self.lock = None


# TODO write more tests
class TodoNoColon:
    pass


# this is a normal comment
def normal_comment():
    pass


def string_literal_marker():
    s = "# TODO not a comment"
    return s


# TODO: refactor cache key
@some_decorator
def single_decorator_todo():
    pass


# FIXME: race on shared state
@first_decorator
@second_decorator
def stacked_decorator_fixme():
    pass


class ClassHeaderComment:
    # WHY: header comment applies to the first method below
    def first_method(self):
        pass


class DecoratedMethods:
    # NOTE: cached for the lifetime of the instance
    @property
    def cached_value(self):
        return 1

    # WHY: must be a classmethod for the registry
    @classmethod
    def registered(cls):
        return cls
