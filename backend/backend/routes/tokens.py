from fastapi import APIRouter, Depends
from sqlmodel import Session

from backend.database import get_db
from backend.models import User
from backend.schemas import TokenResponse
from backend.utils import generate_account_token

router = APIRouter(prefix="/api/token", tags=["tokens"])


@router.post("/new", response_model=TokenResponse)
async def create_new_token(db: Session = Depends(get_db)):
    """Generate a new account token for user registration."""
    account_token = generate_account_token()
    user = User(account_token=account_token)
    db.add(user)
    db.commit()
    db.refresh(user)
    return TokenResponse(
        account_token=account_token, created_at=user.created_at.isoformat()
    )
