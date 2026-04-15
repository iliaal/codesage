MAX_SIZE = 100

def helper(x: int) -> int:
    return x + 1

class UserService:
    def __init__(self, db):
        self.db = db

    def get_user(self, user_id: int):
        return self.db.find(user_id)

    def delete_user(self, user_id: int):
        self.db.delete(user_id)

def standalone():
    pass
