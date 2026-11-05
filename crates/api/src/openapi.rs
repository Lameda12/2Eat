use utoipa::{
    Modify, OpenApi,
    openapi::security::{ApiKey, ApiKeyValue, SecurityScheme},
};

use crate::{dto::*, error::*, routes::*};

struct Security;

impl Modify for Security {
    fn modify(&self, openapi: &mut utoipa::openapi::OpenApi) {
        let c = openapi.components.get_or_insert_with(Default::default);
        c.add_security_scheme(
            "user_id",
            SecurityScheme::ApiKey(ApiKey::Header(ApiKeyValue::with_description(
                "x-user-id",
                "MVP stand-in for authentication: caller's UUID. A cook's user id is their cook id.",
            ))),
        );
        c.add_security_scheme(
            "admin_token",
            SecurityScheme::ApiKey(ApiKey::Header(ApiKeyValue::with_description(
                "x-admin-token",
                "Operator token for certificate review.",
            ))),
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
        cooks::onboarding_terms, cooks::register, cooks::nearby, cooks::cook_dishes,
        dishes::create,
        orders::create, orders::get, orders::accept, orders::ready, orders::complete, orders::cancel, orders::refund,
        admin::get_submission, admin::approve, admin::reject, admin::revoke,
    ),
    components(schemas(
        RegisterCookForm, RegisterCookResponse, OnboardingTerms, NearbyCookResponse,
        CreateDishRequest, DishResponse,
        FulfillmentDto, OrderItemRequest, CreateOrderRequest, RefundRequest, OrderItemResponse, RefundResponse, OrderResponse,
        ApproveCertificationRequest, CookStatusResponse, SubmissionResponse,
        ErrorBody, ErrorDetail,
    )),
    modifiers(&Security),
    tags(
        (name = "cooks"), (name = "dishes"), (name = "orders"),
        (name = "admin", description = "Operator endpoints for certificate review")
    )
)]
pub struct ApiDoc;
