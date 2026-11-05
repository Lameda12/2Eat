use utoipa::{
    Modify, OpenApi,
    openapi::security::{HttpAuthScheme, HttpBuilder, SecurityScheme},
};

use crate::{dto::*, error::*, routes::*};

struct Security;

impl Modify for Security {
    fn modify(&self, openapi: &mut utoipa::openapi::OpenApi) {
        let c = openapi.components.get_or_insert_with(Default::default);
        c.add_security_scheme(
            "bearer_auth",
            SecurityScheme::Http(
                HttpBuilder::new()
                    .scheme(HttpAuthScheme::Bearer)
                    .description(Some("Opaque bearer token from POST /auth/login (valid 30 days, revocable)."))
                    .build(),
            ),
        );
    }
}

#[derive(OpenApi)]
#[openapi(
    info(
        title = "2Eat API",
        description = "Singapore community food marketplace for home-based food businesses (HBFBs). \
            Cooks need a verified Food Safety Course Level 1 (WSQ) certificate to list dishes or accept orders; \
            catering, sales to food retailers and ready-to-eat raw seafood are rejected; every order has a full-refund path.",
        version = "0.1.0"
    ),
    paths(
        auth::register, auth::login, auth::logout, auth::refresh,
        cooks::onboarding_terms, cooks::register, cooks::nearby, cooks::cook_dishes,
        dishes::create,
        orders::create, orders::get, orders::accept, orders::ready, orders::complete, orders::cancel, orders::refund,
        admin::list_reviews, admin::approve_review, admin::reject_review,
        admin::get_submission, admin::approve_cook_certification, admin::reject_cook_certification, admin::revoke,
        admin::flagged_dishes, admin::approve_dish, admin::reject_dish,
        admin::list_refunds, admin::approve_refund, admin::deny_refund,
        admin::audit,
    ),
    components(schemas(
        RegisterCookForm, RegisterCookResponse, OnboardingTerms, NearbyCookResponse,
        CreateDishRequest, DishResponse,
        FulfillmentDto, OrderItemRequest, CreateOrderRequest, RefundRequest, OrderItemResponse, RefundResponse, OrderResponse,
        RegisterRequest, LoginRequest, TokenResponse, UserSummary, MessageResponse,
        RefundAccepted, RefundItem, RefundPage,
        NotesRequest, ApproveCertificationRequest, RejectCertificationRequest, CookStatusResponse, ReviewItem, ReviewPage, DishPage, AuditItem,
        ErrorBody, ErrorDetail,
    )),
    modifiers(&Security),
    tags(
        (name = "auth"), (name = "cooks"), (name = "dishes"), (name = "orders"),
        (name = "admin", description = "Admin-role endpoints: certificate review, flagged dishes, refund review, audit log")
    )
)]
pub struct ApiDoc;
