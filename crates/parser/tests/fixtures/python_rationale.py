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
